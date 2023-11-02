use crate::{common::LOG_PREFIX, registry::Registry};

use std::net::SocketAddr;

use candid::{CandidType, Deserialize};
#[cfg(target_arch = "wasm32")]
use dfn_core::println;

use ic_base_types::NodeId;
use ic_crypto_node_key_validation::ValidNodePublicKeys;
use ic_crypto_utils_basic_sig::conversions as crypto_basicsig_conversions;
use ic_protobuf::registry::{
    crypto::v1::{PublicKey, X509PublicKeyCert},
    node::v1::{ConnectionEndpoint, FlowEndpoint, NodeRecord},
};

use crate::mutations::node_management::common::{
    get_node_operator_record, make_add_node_registry_mutations, make_update_node_operator_mutation,
    scan_for_nodes_by_ip,
};
use crate::mutations::node_management::do_remove_node_directly::RemoveNodeDirectlyPayload;
use ic_types::crypto::CurrentNodePublicKeys;
use ic_types::time::Time;
use prost::Message;

impl Registry {
    /// Adds a new node to the registry.
    ///
    /// This method is called directly by the node or tool that needs to
    /// add a node.
    pub fn do_add_node(&mut self, payload: AddNodePayload) -> Result<NodeId, String> {
        println!(
            "{}do_add_node started: {:?} caller: {:?}",
            LOG_PREFIX,
            payload,
            dfn_core::api::caller()
        );

        // The steps are now:
        // 1. get the caller ID and check if it is in the registry
        let caller = dfn_core::api::caller();

        let mut node_operator_record = get_node_operator_record(self, caller)
            .map_err(|err| format!("{}do_add_node: Aborting node addition: {}", LOG_PREFIX, err))
            .unwrap();

        // 2. Clear out any nodes that already exist at this IP.
        // This will only succeed if:
        // - the same NO was in control of the original nodes.
        // - the nodes are no longer in subnets.
        //
        // (We use the http endpoint to be in line with what is used by the
        // release dashboard.)
        let http_endpoint = connection_endpoint_from_string(&payload.http_endpoint);
        let nodes_with_same_ip = scan_for_nodes_by_ip(self, &http_endpoint.ip_addr);
        if !nodes_with_same_ip.is_empty() {
            for node_id in nodes_with_same_ip {
                self.do_remove_node_directly(RemoveNodeDirectlyPayload { node_id });
            }

            // Update the NO record, as the available allowance may have changed.
            node_operator_record = get_node_operator_record(self, caller)
                .map_err(|err| {
                    format!("{}do_add_node: Aborting node addition: {}", LOG_PREFIX, err)
                })
                .unwrap();
        }

        // 3. check if adding one more node will get us over the cap for the Node
        // Operator
        if node_operator_record.node_allowance == 0 {
            return Err("Node allowance for this Node Operator is exhausted".to_string());
        }

        // 4. Validate keys and get the node id
        let (node_id, valid_pks) = valid_keys_from_payload(&payload)?;

        println!("{}do_add_node: The node id is {:?}", LOG_PREFIX, node_id);

        let mut p2p_endpoint = connection_endpoint_from_string(&payload.http_endpoint);
        p2p_endpoint.port = 4100;
        // 5. create the Node Record
        let node_record = NodeRecord {
            xnet: Some(connection_endpoint_from_string(&payload.xnet_endpoint)),
            http: Some(connection_endpoint_from_string(&payload.http_endpoint)),
            p2p_flow_endpoints: vec![FlowEndpoint {
                endpoint: Some(p2p_endpoint),
            }],
            node_operator_id: caller.into_vec(),
            chip_id: vec![],
            hostos_version_id: None,
        };

        // 6. Insert node, public keys, and crypto keys
        let mut mutations = make_add_node_registry_mutations(node_id, node_record, valid_pks);

        // Update the Node Operator record
        let mut node_operator_record = node_operator_record;
        node_operator_record.node_allowance -= 1;

        let update_node_operator_record =
            make_update_node_operator_mutation(caller, &node_operator_record);

        mutations.push(update_node_operator_record);

        // Check invariants before applying mutations
        self.maybe_apply_mutation_internal(mutations);

        println!("{}do_add_node finished: {:?}", LOG_PREFIX, payload);

        Ok(node_id)
    }
}

/// The payload of an update request to add a new node.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AddNodePayload {
    // Raw bytes of the protobuf, but these should be PublicKey
    pub node_signing_pk: Vec<u8>,
    pub committee_signing_pk: Vec<u8>,
    pub ni_dkg_dealing_encryption_pk: Vec<u8>,
    // Raw bytes of the protobuf, but these should be X509PublicKeyCert
    pub transport_tls_cert: Vec<u8>,
    // Raw bytes of the protobuf, but these should be PublicKey
    pub idkg_dealing_encryption_pk: Option<Vec<u8>>,

    pub xnet_endpoint: String,
    pub http_endpoint: String,

    // TODO(NNS1-2444): The fields below are deprecated and they are not read anywhere.
    pub p2p_flow_endpoints: Vec<String>,
    pub prometheus_metrics_endpoint: String,
}

/// Parses the ConnectionEndpoint string
///
/// The string is written in form: `ipv4:port` or `[ipv6]:port`.
pub fn connection_endpoint_from_string(endpoint: &str) -> ConnectionEndpoint {
    match endpoint.parse::<SocketAddr>() {
        Err(e) => panic!(
            "Could not convert {:?} to a connection endpoint: {:?}",
            endpoint, e
        ),
        Ok(sa) => ConnectionEndpoint {
            ip_addr: sa.ip().to_string(),
            port: sa.port() as u32, // because protobufs don't have u16
        },
    }
}

/// Parses a P2P flow encoded in a string
///
/// The string is written in form: `flow,ipv4:port` or `flow,[ipv6]:port`.
pub fn flow_endpoint_from_string(endpoint: &str) -> FlowEndpoint {
    let parts = endpoint.splitn(2, ',').collect::<Vec<&str>>();
    parts[0].parse::<u32>().unwrap();
    println!("Parts are {:?} and {:?}", parts[0], parts[1]);
    match parts[1].parse::<SocketAddr>() {
        Err(e) => panic!(
            "Could not convert {:?} to a connection endpoint: {:?}",
            endpoint, e
        ),
        Ok(sa) => FlowEndpoint {
            endpoint: Some(ConnectionEndpoint {
                ip_addr: sa.ip().to_string(),
                port: sa.port() as u32, // because protobufs don't have u16
            }),
        },
    }
}

/// Validates the payload and extracts node's public keys
fn valid_keys_from_payload(
    payload: &AddNodePayload,
) -> Result<(NodeId, ValidNodePublicKeys), String> {
    // 1. verify that the keys we got are not empty
    if payload.node_signing_pk.is_empty() {
        return Err(String::from("node_signing_pk is empty"));
    };
    if payload.committee_signing_pk.is_empty() {
        return Err(String::from("committee_signing_pk is empty"));
    };
    if payload.ni_dkg_dealing_encryption_pk.is_empty() {
        return Err(String::from("ni_dkg_dealing_encryption_pk is empty"));
    };
    if payload.transport_tls_cert.is_empty() {
        return Err(String::from("transport_tls_cert is empty"));
    };
    // TODO(NNS1-1197): Refactor this when nodes are provisioned for threshold ECDSA subnets
    if let Some(idkg_dealing_encryption_pk) = &payload.idkg_dealing_encryption_pk {
        if idkg_dealing_encryption_pk.is_empty() {
            return Err(String::from("idkg_dealing_encryption_pk is empty"));
        };
    }

    // 2. get the keys for verification -- for that, we need to create
    // NodePublicKeys first
    let node_signing_pk = PublicKey::decode(&payload.node_signing_pk[..])
        .map_err(|e| format!("node_signing_pk is not in the expected format: {:?}", e))?;
    let committee_signing_pk =
        PublicKey::decode(&payload.committee_signing_pk[..]).map_err(|e| {
            format!(
                "committee_signing_pk is not in the expected format: {:?}",
                e
            )
        })?;
    let tls_certificate = X509PublicKeyCert::decode(&payload.transport_tls_cert[..])
        .map_err(|e| format!("transport_tls_cert is not in the expected format: {:?}", e))?;
    let dkg_dealing_encryption_pk = PublicKey::decode(&payload.ni_dkg_dealing_encryption_pk[..])
        .map_err(|e| {
            format!(
                "ni_dkg_dealing_encryption_pk is not in the expected format: {:?}",
                e
            )
        })?;
    // TODO(NNS1-1197): Refactor when nodes are provisioned for threshold ECDSA subnets
    let idkg_dealing_encryption_pk =
        if let Some(idkg_de_pk_bytes) = &payload.idkg_dealing_encryption_pk {
            Some(PublicKey::decode(&idkg_de_pk_bytes[..]).map_err(|e| {
                format!(
                    "idkg_dealing_encryption_pk is not in the expected format: {:?}",
                    e
                )
            })?)
        } else {
            None
        };

    // 3. get the node id from the node_signing_pk
    let node_id = crypto_basicsig_conversions::derive_node_id(&node_signing_pk).map_err(|e| {
        format!(
            "node signing public key couldn't be converted to a NodeId: {:?}",
            e
        )
    })?;

    // 4. get the keys for verification -- for that, we need to create
    let node_pks = CurrentNodePublicKeys {
        node_signing_public_key: Some(node_signing_pk),
        committee_signing_public_key: Some(committee_signing_pk),
        tls_certificate: Some(tls_certificate),
        dkg_dealing_encryption_public_key: Some(dkg_dealing_encryption_pk),
        idkg_dealing_encryption_public_key: idkg_dealing_encryption_pk,
    };

    // 5. validate the keys and the node_id
    match ValidNodePublicKeys::try_from(node_pks, node_id, now()?) {
        Ok(valid_pks) => Ok((node_id, valid_pks)),
        Err(e) => Err(format!("Could not validate public keys, due to {:?}", e)),
    }
}

fn now() -> Result<Time, String> {
    let duration = dfn_core::api::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("Could not get current time since UNIX_EPOCH: {e}"))?;

    let nanos = u64::try_from(duration.as_nanos())
        .map_err(|e| format!("Current time cannot be converted to u64: {:?}", e))?;

    Ok(Time::from_nanos_since_unix_epoch(nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_config::crypto::CryptoConfig;
    use ic_crypto_node_key_generation::generate_node_keys_once;
    use ic_nns_common::registry::encode_or_panic;
    use lazy_static::lazy_static;

    #[derive(Clone)]
    struct TestData {
        node_pks: ValidNodePublicKeys,
    }

    impl TestData {
        fn new() -> Self {
            let (config, _temp_dir) = CryptoConfig::new_in_temp_dir();
            Self {
                node_pks: generate_node_keys_once(&config, None)
                    .expect("error generating node public keys"),
            }
        }
    }

    // This is to avoid calling the expensive key generation operation for every
    // test.
    lazy_static! {
        static ref TEST_DATA: TestData = TestData::new();
        static ref PAYLOAD: AddNodePayload = AddNodePayload {
            node_signing_pk: vec![],
            committee_signing_pk: vec![],
            ni_dkg_dealing_encryption_pk: vec![],
            transport_tls_cert: vec![],
            idkg_dealing_encryption_pk: Some(vec![]),
            xnet_endpoint: "127.0.0.1:1234".to_string(),
            http_endpoint: "127.0.0.1:8123".to_string(),
            p2p_flow_endpoints: vec![],
            prometheus_metrics_endpoint: "".to_string(),
        };
    }

    #[test]
    fn empty_node_signing_key_is_detected() {
        let payload = PAYLOAD.clone();
        assert!(valid_keys_from_payload(&payload).is_err());
    }

    #[test]
    fn empty_committee_signing_key_is_detected() {
        let mut payload = PAYLOAD.clone();
        let node_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.node_signing_key());
        payload.node_signing_pk = node_signing_pubkey;
        assert!(valid_keys_from_payload(&payload).is_err());
    }

    #[test]
    fn empty_dkg_dealing_key_is_detected() {
        let mut payload = PAYLOAD.clone();
        let node_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.node_signing_key());
        let committee_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.committee_signing_key());
        payload.node_signing_pk = node_signing_pubkey;
        payload.committee_signing_pk = committee_signing_pubkey;
        assert!(valid_keys_from_payload(&payload).is_err());
    }

    #[test]
    fn empty_tls_cert_is_detected() {
        let mut payload = PAYLOAD.clone();
        let node_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.node_signing_key());
        let committee_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.committee_signing_key());
        let ni_dkg_dealing_encryption_pubkey =
            encode_or_panic(TEST_DATA.node_pks.dkg_dealing_encryption_key());
        payload.node_signing_pk = node_signing_pubkey;
        payload.committee_signing_pk = committee_signing_pubkey;
        payload.ni_dkg_dealing_encryption_pk = ni_dkg_dealing_encryption_pubkey;
        assert!(valid_keys_from_payload(&payload).is_err());
    }

    #[test]
    fn empty_idkg_key_is_detected() {
        let mut payload = PAYLOAD.clone();
        let node_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.node_signing_key());
        let committee_signing_pubkey = encode_or_panic(TEST_DATA.node_pks.committee_signing_key());
        let ni_dkg_dealing_encryption_pubkey =
            encode_or_panic(TEST_DATA.node_pks.dkg_dealing_encryption_key());
        let tls_certificate = encode_or_panic(TEST_DATA.node_pks.tls_certificate());
        payload.node_signing_pk = node_signing_pubkey;
        payload.committee_signing_pk = committee_signing_pubkey;
        payload.ni_dkg_dealing_encryption_pk = ni_dkg_dealing_encryption_pubkey;
        payload.transport_tls_cert = tls_certificate;
        assert!(valid_keys_from_payload(&payload).is_err());
    }

    #[test]
    #[should_panic]
    fn empty_string_causes_panic() {
        connection_endpoint_from_string("");
    }

    #[test]
    #[should_panic]
    fn no_port_causes_panic() {
        connection_endpoint_from_string("0.0.0.0:");
    }

    #[test]
    #[should_panic]
    fn no_addr_causes_panic() {
        connection_endpoint_from_string(":1234");
    }

    #[test]
    #[should_panic]
    fn bad_addr_causes_panic() {
        connection_endpoint_from_string("xyz:1234");
    }

    #[test]
    #[should_panic]
    fn ipv6_no_brackets_causes_panic() {
        connection_endpoint_from_string("::1:1234");
    }

    #[test]
    fn good_ipv4() {
        assert_eq!(
            connection_endpoint_from_string("192.168.1.3:8080"),
            ConnectionEndpoint {
                ip_addr: "192.168.1.3".to_string(),
                port: 8080u32,
            }
        );
    }

    #[test]
    #[should_panic]
    fn bad_ipv4_port() {
        connection_endpoint_from_string("192.168.1.3:80800");
    }

    #[test]
    fn good_ipv6() {
        assert_eq!(
            connection_endpoint_from_string("[fe80::1]:80"),
            ConnectionEndpoint {
                ip_addr: "fe80::1".to_string(),
                port: 80u32,
            }
        );
    }

    #[test]
    #[should_panic]
    fn no_flow_id_causes_panic() {
        flow_endpoint_from_string("127.0.0.1:8080");
    }

    #[test]
    #[should_panic]
    fn empty_flow_endpoint_string_causes_panic() {
        flow_endpoint_from_string("");
    }

    #[test]
    #[should_panic]
    fn non_numeric_flow_id_causes_panic() {
        flow_endpoint_from_string("abcd,127.0.0.1:8080");
    }

    #[test]
    fn good_flow_id_ipv4() {
        assert_eq!(
            flow_endpoint_from_string("1337,127.0.0.1:8080"),
            FlowEndpoint {
                endpoint: Some(ConnectionEndpoint {
                    ip_addr: "127.0.0.1".to_string(),
                    port: 8080u32,
                })
            }
        );
    }
}
