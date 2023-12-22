use candid::Encode;
use dfn_candid::candid_one;
use ic_base_types::{CanisterId, PrincipalId};
use ic_nervous_system_clients::{
    canister_id_record::CanisterIdRecord, canister_status::CanisterStatusResult,
};
use ic_nns_constants::LIFELINE_CANISTER_INDEX_IN_NNS_SUBNET;
use ic_nns_governance::pb::v1::{
    manage_neuron_response::{Command, MakeProposalResponse},
    proposal::Action,
    ExecuteNnsFunction, NnsFunction, Proposal,
};
use ic_nns_test_utils::{
    common::NnsInitPayloadsBuilder,
    neuron_helpers::get_neuron_1,
    state_test_helpers::{nns_governance_make_proposal, setup_nns_canisters, update_with_sender},
};
use ic_state_machine_tests::StateMachine;

/*
Title:: Uninstall a canister from a subnet via proposal

Goal:: Ensure that canisters can be uninstalled via proposals submitted to the Governance Canister.

Runbook::
. Setup: StateMachine of the replica with installed NNS canisters.
. Assert that `update` call executes successfully on a test canister (lifeline_canister).
. Submit a proposal to the Governance Canister to uninstall the test canister code.
. Assert that `update` call fails on the test canister.

Success::
. Update call executes successfully on the test canister after its installation.
. Update call fails on the test canister after the proposal to uninstall code of this canister is executed.
*/

fn setup_state_machine_with_nns_canisters() -> StateMachine {
    let state_machine = StateMachine::new();
    let nns_init_payloads = NnsInitPayloadsBuilder::new().with_test_neurons().build();
    setup_nns_canisters(&state_machine, nns_init_payloads);
    state_machine
}

#[test]
fn uninstall_canister_by_proposal() {
    let mut state_machine = setup_state_machine_with_nns_canisters();
    // Pick some installed nns canister for testing
    let canister_id = CanisterId::from_u64(LIFELINE_CANISTER_INDEX_IN_NNS_SUBNET);
    // Confirm that canister exists and has some code installed
    assert!(state_machine.canister_exists(canister_id));
    let status: Result<CanisterStatusResult, String> = update_with_sender(
        &state_machine,
        canister_id,
        "canister_status",
        candid_one,
        &CanisterIdRecord::from(canister_id),
        PrincipalId::new_anonymous(),
    );
    assert!(status.unwrap().module_hash.is_some());
    // Prepare a proposal to uninstall canister code
    let proposal = Proposal {
        title: Some("<proposal to uninstall an NNS canister>".to_string()),
        summary: "".to_string(),
        url: "".to_string(),
        action: Some(Action::ExecuteNnsFunction(ExecuteNnsFunction {
            nns_function: NnsFunction::UninstallCode as i32,
            payload: Encode!(&CanisterIdRecord { canister_id })
                .expect("Error encoding proposal payload"),
        })),
    };
    // To make a proposal we need a neuron
    let n1 = get_neuron_1();
    // Execute a proposal
    let response =
        nns_governance_make_proposal(&mut state_machine, n1.principal_id, n1.neuron_id, &proposal)
            .command
            .expect("Making NNS proposal failed");
    let _proposal_id = match response {
        Command::MakeProposal(MakeProposalResponse {
            proposal_id: Some(ic_nns_common::pb::v1::ProposalId { id }),
        }) => id,
        _ => panic!("Response did not contain a proposal_id: {:#?}", response),
    };
    // Verify that now calling a canister method fails.
    let status: Result<CanisterStatusResult, String> = update_with_sender(
        &state_machine,
        canister_id,
        "canister_status",
        candid_one,
        &CanisterIdRecord::from(canister_id),
        PrincipalId::new_anonymous(),
    );
    assert!(status.err().unwrap().to_lowercase().contains("no wasm"));
    // Canister itself should still exist though
    assert!(state_machine.canister_exists(canister_id));
}
