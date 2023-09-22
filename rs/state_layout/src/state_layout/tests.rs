use super::*;

use ic_ic00_types::{
    CanisterChange, CanisterChangeDetails, CanisterChangeOrigin, CanisterInstallMode, IC_00,
};
use ic_replicated_state::canister_state::system_state::CanisterHistory;
use ic_replicated_state::metadata_state::subnet_call_context_manager::InstallCodeCallId;
use ic_test_utilities::types::ids::user_test_id;
use ic_test_utilities::{
    mock_time,
    types::{
        ids::canister_test_id,
        messages::{IngressBuilder, RequestBuilder, ResponseBuilder},
    },
};
use ic_test_utilities_logger::with_test_replica_logger;
use ic_test_utilities_tmpdir::tmpdir;
use ic_types::messages::{CanisterCall, CanisterMessage, CanisterMessageOrTask};
use std::sync::Arc;

fn default_canister_state_bits() -> CanisterStateBits {
    CanisterStateBits {
        controllers: BTreeSet::new(),
        last_full_execution_round: ExecutionRound::from(0),
        call_context_manager: None,
        compute_allocation: ComputeAllocation::try_from(0).unwrap(),
        accumulated_priority: AccumulatedPriority::default(),
        execution_state_bits: None,
        memory_allocation: MemoryAllocation::default(),
        freeze_threshold: NumSeconds::from(0),
        cycles_balance: Cycles::zero(),
        cycles_debit: Cycles::zero(),
        reserved_balance: Cycles::zero(),
        status: CanisterStatus::Stopped,
        scheduled_as_first: 0,
        skipped_round_due_to_no_messages: 0,
        executed: 0,
        interruped_during_execution: 0,
        certified_data: vec![],
        consumed_cycles_since_replica_started: NominalCycles::from(0),
        stable_memory_size: NumWasmPages::from(0),
        heap_delta_debit: NumBytes::from(0),
        install_code_debit: NumInstructions::from(0),
        time_of_last_allocation_charge_nanos: mock_time().as_nanos_since_unix_epoch(),
        task_queue: vec![],
        global_timer_nanos: None,
        canister_version: 0,
        consumed_cycles_since_replica_started_by_use_cases: BTreeMap::new(),
        canister_history: CanisterHistory::default(),
    }
}

#[test]
fn test_state_layout_diverged_state_paths() {
    with_test_replica_logger(|log| {
        let tempdir = tmpdir("state_layout");
        let root_path = tempdir.path().to_path_buf();
        let metrics_registry = ic_metrics::MetricsRegistry::new();
        let state_layout = StateLayout::try_new(log, root_path.clone(), &metrics_registry).unwrap();
        state_layout
            .create_diverged_state_marker(Height::new(1))
            .unwrap();
        assert_eq!(
            state_layout.diverged_state_heights().unwrap(),
            vec![Height::new(1)],
        );
        assert!(state_layout
            .diverged_state_marker_path(Height::new(1))
            .starts_with(root_path.join("diverged_state_markers")));
        state_layout
            .remove_diverged_state_marker(Height::new(1))
            .unwrap();
        assert!(state_layout.diverged_state_heights().unwrap().is_empty());
    });
}

#[test]
fn test_encode_decode_empty_controllers() {
    // A canister state with empty controllers.
    let canister_state_bits = default_canister_state_bits();

    let pb_bits = pb_canister_state_bits::CanisterStateBits::from(canister_state_bits);
    let canister_state_bits = CanisterStateBits::try_from(pb_bits).unwrap();

    // Controllers are still empty, as expected.
    assert_eq!(canister_state_bits.controllers, BTreeSet::new());
}

#[test]
fn test_encode_decode_non_empty_controllers() {
    let mut controllers = BTreeSet::new();
    controllers.insert(IC_00.into());
    controllers.insert(canister_test_id(0).get());

    // A canister state with non-empty controllers.
    let canister_state_bits = CanisterStateBits {
        controllers,
        ..default_canister_state_bits()
    };

    let pb_bits = pb_canister_state_bits::CanisterStateBits::from(canister_state_bits);
    let canister_state_bits = CanisterStateBits::try_from(pb_bits).unwrap();

    let mut expected_controllers = BTreeSet::new();
    expected_controllers.insert(canister_test_id(0).get());
    expected_controllers.insert(IC_00.into());
    assert_eq!(canister_state_bits.controllers, expected_controllers);
}

#[test]
fn test_encode_decode_empty_history() {
    let canister_history = CanisterHistory::default();

    // A canister state with empty history.
    let canister_state_bits = CanisterStateBits {
        canister_history: canister_history.clone(),
        ..default_canister_state_bits()
    };

    let pb_bits = pb_canister_state_bits::CanisterStateBits::from(canister_state_bits);
    let canister_state_bits = CanisterStateBits::try_from(pb_bits).unwrap();

    assert_eq!(canister_state_bits.canister_history, canister_history);
}

#[test]
fn test_encode_decode_non_empty_history() {
    let mut canister_history = CanisterHistory::default();
    canister_history.add_canister_change(CanisterChange::new(
        42,
        0,
        CanisterChangeOrigin::from_user(user_test_id(42).get()),
        CanisterChangeDetails::canister_creation(vec![
            canister_test_id(777).get(),
            user_test_id(42).get(),
        ]),
    ));
    canister_history.add_canister_change(CanisterChange::new(
        123,
        1,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), None),
        CanisterChangeDetails::CanisterCodeUninstall,
    ));
    canister_history.add_canister_change(CanisterChange::new(
        222,
        2,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), Some(777)),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Install, [0; 32]),
    ));
    canister_history.add_canister_change(CanisterChange::new(
        222,
        3,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), Some(888)),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Upgrade, [1; 32]),
    ));
    canister_history.add_canister_change(CanisterChange::new(
        222,
        4,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), Some(999)),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Reinstall, [2; 32]),
    ));
    canister_history.add_canister_change(CanisterChange::new(
        333,
        5,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), None),
        CanisterChangeDetails::controllers_change(vec![
            canister_test_id(123).into(),
            user_test_id(666).get(),
        ]),
    ));
    canister_history.add_canister_change(CanisterChange::new(
        444,
        6,
        CanisterChangeOrigin::from_canister(canister_test_id(123).get(), None),
        CanisterChangeDetails::controllers_change(vec![]),
    ));

    // A canister state with non-empty history.
    let canister_state_bits = CanisterStateBits {
        canister_history: canister_history.clone(),
        ..default_canister_state_bits()
    };

    let pb_bits = pb_canister_state_bits::CanisterStateBits::from(canister_state_bits);
    let canister_state_bits = CanisterStateBits::try_from(pb_bits).unwrap();

    assert_eq!(canister_state_bits.canister_history, canister_history);
}

#[test]
fn test_encode_decode_task_queue() {
    let ingress = Arc::new(IngressBuilder::new().method_name("test_ingress").build());
    let request = Arc::new(RequestBuilder::new().method_name("test_request").build());
    let response = Arc::new(
        ResponseBuilder::new()
            .respondent(canister_test_id(42))
            .build(),
    );
    let task_queue = vec![
        ExecutionTask::AbortedInstallCode {
            message: CanisterCall::Ingress(Arc::clone(&ingress)),
            prepaid_execution_cycles: Cycles::new(1),
            call_id: None,
        },
        ExecutionTask::AbortedExecution {
            input: CanisterMessageOrTask::Message(CanisterMessage::Request(Arc::clone(&request))),
            prepaid_execution_cycles: Cycles::new(2),
        },
        ExecutionTask::AbortedInstallCode {
            message: CanisterCall::Request(Arc::clone(&request)),
            prepaid_execution_cycles: Cycles::new(3),
            call_id: Some(InstallCodeCallId::new(3u64)),
        },
        ExecutionTask::AbortedExecution {
            input: CanisterMessageOrTask::Message(CanisterMessage::Response(Arc::clone(&response))),
            prepaid_execution_cycles: Cycles::new(4),
        },
        ExecutionTask::AbortedExecution {
            input: CanisterMessageOrTask::Message(CanisterMessage::Ingress(Arc::clone(&ingress))),
            prepaid_execution_cycles: Cycles::new(5),
        },
    ];
    let canister_state_bits = CanisterStateBits {
        task_queue: task_queue.clone(),
        ..default_canister_state_bits()
    };

    let pb_bits = pb_canister_state_bits::CanisterStateBits::from(canister_state_bits);
    let canister_state_bits = CanisterStateBits::try_from(pb_bits).unwrap();
    assert_eq!(canister_state_bits.task_queue, task_queue);
}

#[test]
fn test_removal_when_last_dropped() {
    with_test_replica_logger(|log| {
        let tempdir = tmpdir("state_layout");
        let root_path = tempdir.path().to_path_buf();
        let metrics_registry = ic_metrics::MetricsRegistry::new();
        let state_layout = StateLayout::try_new(log, root_path, &metrics_registry).unwrap();
        let scratchpad_dir = tmpdir("scratchpad");
        let cp1 = state_layout
            .scratchpad_to_checkpoint(
                CheckpointLayout::<RwPolicy<()>>::new_untracked(
                    scratchpad_dir.path().to_path_buf().join("1"),
                    Height::new(1),
                )
                .unwrap(),
                Height::new(1),
                None,
            )
            .unwrap();
        let cp2 = state_layout
            .scratchpad_to_checkpoint(
                CheckpointLayout::<RwPolicy<()>>::new_untracked(
                    scratchpad_dir.path().to_path_buf().join("2"),
                    Height::new(2),
                )
                .unwrap(),
                Height::new(2),
                None,
            )
            .unwrap();
        // Add one checkpoint so that we never remove the last one and crash
        let _cp3 = state_layout
            .scratchpad_to_checkpoint(
                CheckpointLayout::<RwPolicy<()>>::new_untracked(
                    scratchpad_dir.path().to_path_buf().join("3"),
                    Height::new(3),
                )
                .unwrap(),
                Height::new(3),
                None,
            )
            .unwrap();
        assert_eq!(
            vec![Height::new(1), Height::new(2), Height::new(3)],
            state_layout.checkpoint_heights().unwrap(),
        );

        std::mem::drop(cp1);
        state_layout.remove_checkpoint_when_unused(Height::new(1));
        state_layout.remove_checkpoint_when_unused(Height::new(2));
        assert_eq!(
            vec![Height::new(2), Height::new(3)],
            state_layout.checkpoint_heights().unwrap(),
        );

        std::mem::drop(cp2);
        assert_eq!(
            vec![Height::new(3)],
            state_layout.checkpoint_heights().unwrap(),
        );
    });
}

#[test]
#[should_panic]
#[cfg(debug_assertions)]
fn test_last_removal_panics_in_debug() {
    with_test_replica_logger(|log| {
        let tempdir = tmpdir("state_layout");
        let root_path = tempdir.path().to_path_buf();
        let metrics_registry = ic_metrics::MetricsRegistry::new();
        let state_layout = StateLayout::try_new(log, root_path, &metrics_registry).unwrap();
        let scratchpad_dir = tmpdir("scratchpad");
        let cp1 = state_layout
            .scratchpad_to_checkpoint(
                CheckpointLayout::<RwPolicy<()>>::new_untracked(
                    scratchpad_dir.path().to_path_buf().join("1"),
                    Height::new(1),
                )
                .unwrap(),
                Height::new(1),
                None,
            )
            .unwrap();
        state_layout.remove_checkpoint_when_unused(Height::new(1));
        std::mem::drop(cp1);
    });
}

#[test]
fn test_canister_id_from_path() {
    assert_eq!(
        Some(CanisterId::from_u64(1)),
        canister_id_from_path(Path::new(
            "canister_states/00000000000000010101/canister.pbuf"
        ))
    );
    assert_eq!(
        Some(CanisterId::from_u64(2)),
        canister_id_from_path(Path::new(
            "canister_states/00000000000000020101/queues.pbuf"
        ))
    );
    assert_eq!(
        None,
        canister_id_from_path(Path::new(
            "foo/canister_states/00000000000000030101/queues.pbuf"
        ))
    );
    assert_eq!(None, canister_id_from_path(Path::new(SUBNET_QUEUES_FILE)));
    assert_eq!(None, canister_id_from_path(Path::new("canister_states")));
    assert_eq!(
        None,
        canister_id_from_path(Path::new("canister_states/not-a-canister-ID/queues.pbuf"))
    );
}
