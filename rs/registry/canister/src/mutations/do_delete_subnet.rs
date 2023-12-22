use std::convert::TryFrom;

use crate::{
    common::LOG_PREFIX,
    mutations::common::{decode_registry_value, encode_or_panic},
    registry::Registry,
};

use candid::{CandidType, Deserialize};
use cycles_minting_canister::RemoveSubnetFromAuthorizedSubnetListArgs;
use dfn_core::call;
use ic_base_types::{PrincipalId, SubnetId};
use ic_nns_constants::{CYCLES_MINTING_CANISTER_ID, GOVERNANCE_CANISTER_ID};
use ic_protobuf::registry::routing_table::v1::RoutingTable as RoutingTablePb;
use ic_registry_keys::{
    make_catch_up_package_contents_key, make_crypto_threshold_signing_pubkey_key,
    make_firewall_rules_record_key, make_routing_table_record_key, make_subnet_list_record_key,
    make_subnet_record_key, FirewallRulesScope,
};
use ic_registry_routing_table::RoutingTable;
use ic_registry_transport::{delete, update};
use lazy_static::lazy_static;
use std::str::FromStr;

lazy_static! {
    pub static ref NNS_SUBNET_ID: SubnetId = SubnetId::new(
        PrincipalId::from_str("tdb26-jop6k-aogll-7ltgs-eruif-6kk7m-qpktf-gdiqx-mxtrf-vb5e6-eqe")
            .unwrap()
    );
}

impl Registry {
    /// Delete an existing Subnet from the Registry.
    ///
    /// This method is called by Governance, after a proposal for deleting a
    /// Subnet has been accepted.
    pub async fn do_delete_subnet(&mut self, payload: DeleteSubnetPayload) {
        println!("{}do_delete_subnet: {:?}", LOG_PREFIX, payload);

        let subnet_id_to_remove = SubnetId::from(payload.subnet_id.unwrap());
        let mut subnet_list = self.get_subnet_list_record();

        let latest_version = self.latest_version();

        let routing_table_vec = self
            .get(make_routing_table_record_key().as_bytes(), latest_version)
            .unwrap();
        let routing_table = RoutingTable::try_from(decode_registry_value::<RoutingTablePb>(
            routing_table_vec.value.clone(),
        ))
        .unwrap();
        let nns_subnet_id = match routing_table.route(GOVERNANCE_CANISTER_ID.into()) {
            Some(v) => v,
            None => *NNS_SUBNET_ID,
        };

        // Check that the Subnet hosting the governance canister will not be deleted
        if subnet_id_to_remove == nns_subnet_id {
            panic!("Cannot delete the NNS subnet");
        }

        if !subnet_list
            .subnets
            .contains(&subnet_id_to_remove.get().into())
        {
            panic!("Subnet {} does not exist", subnet_id_to_remove);
        }

        // 1. Remove Subnet from Routing Table
        let update_routing_table_mutation =
            self.remove_subnet_from_routing_table(latest_version, subnet_id_to_remove);

        // 2. Remove Subnet from Subnet List
        subnet_list
            .subnets
            .retain(|subnet_id| *subnet_id != subnet_id_to_remove.get().to_vec());
        let update_subnet_list_mutation = update(
            make_subnet_list_record_key().as_bytes(),
            encode_or_panic(&subnet_list),
        );

        // 3. Delete Subnet's CUP
        let delete_cup_mutation =
            delete(make_catch_up_package_contents_key(subnet_id_to_remove).as_bytes());

        // 4. Delete Subnet's threshold signing key
        let delete_threshold_signing_pubkey =
            delete(make_crypto_threshold_signing_pubkey_key(subnet_id_to_remove).as_bytes());

        // 5. Delete Subnet record
        let delete_subnet_mutation = delete(make_subnet_record_key(subnet_id_to_remove));

        let mut mutations = vec![
            update_routing_table_mutation,
            update_subnet_list_mutation,
            delete_cup_mutation,
            delete_threshold_signing_pubkey,
            delete_subnet_mutation,
        ];

        // 6. Delete Subnet specific Firewall Ruleset (if it exists)
        let firewall_ruleset_key =
            make_firewall_rules_record_key(&FirewallRulesScope::Subnet(subnet_id_to_remove));
        if self
            .get(firewall_ruleset_key.as_bytes(), self.latest_version())
            .is_some()
        {
            let delete_firewall_ruleset_mutation = delete(firewall_ruleset_key);
            mutations.push(delete_firewall_ruleset_mutation);
        }

        // 7. Make a call to the CMC and only apply changes if it's successful
        let cmc_payload = RemoveSubnetFromAuthorizedSubnetListArgs {
            subnet: subnet_id_to_remove,
        };
        let _: () = call(
            CYCLES_MINTING_CANISTER_ID,
            "remove_subnet_from_authorized_subnet_list",
            dfn_candid::candid_one,
            &cmc_payload,
        )
        .await
        .expect("Call to the CMC did not succeed, subnet deletion reverted");

        // 8. Check invariants and apply mutations
        self.maybe_apply_mutation_internal(mutations);
    }
}

/// The payload of a proposal to delete an existing subnet.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeleteSubnetPayload {
    pub subnet_id: Option<PrincipalId>,
}
