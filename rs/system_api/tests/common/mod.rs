use std::{convert::TryFrom, sync::Arc};

use ic_base_types::{CanisterId, NumBytes, SubnetId};
use ic_config::{
    embedders::Config as EmbeddersConfig, flag_status::FlagStatus, subnet_config::SchedulerConfig,
};
use ic_cycles_account_manager::{CyclesAccountManager, ResourceSaturation};
use ic_ic00_types::IC_00;
use ic_interfaces::execution_environment::{ExecutionMode, SubnetAvailableMemory};
use ic_logger::replica_logger::no_op_logger;
use ic_nns_constants::CYCLES_MINTING_CANISTER_ID;
use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{CallOrigin, Memory, NetworkTopology, SubnetTopology, SystemState};
use ic_system_api::{
    sandbox_safe_system_state::SandboxSafeSystemState, ApiType, DefaultOutOfInstructionsHandler,
    ExecutionParameters, InstructionLimits, SystemApiImpl,
};
use ic_test_utilities::{
    mock_time,
    state::SystemStateBuilder,
    types::ids::{call_context_test_id, canister_test_id, subnet_test_id, user_test_id},
};
use ic_types::{
    messages::{CallContextId, CallbackId, RejectContext},
    methods::SystemMethod,
    ComputeAllocation, Cycles, MemoryAllocation, NumInstructions, PrincipalId, Time,
    MAX_WASM_MEMORY_IN_BYTES,
};
use maplit::btreemap;

pub const CANISTER_CURRENT_MEMORY_USAGE: NumBytes = NumBytes::new(0);

const SUBNET_MEMORY_CAPACITY: i64 = i64::MAX / 2;

pub fn execution_parameters() -> ExecutionParameters {
    ExecutionParameters {
        instruction_limits: InstructionLimits::new(
            FlagStatus::Disabled,
            NumInstructions::from(5_000_000_000),
            NumInstructions::from(5_000_000_000),
        ),
        canister_memory_limit: NumBytes::new(MAX_WASM_MEMORY_IN_BYTES),
        memory_allocation: MemoryAllocation::default(),
        compute_allocation: ComputeAllocation::default(),
        subnet_type: SubnetType::Application,
        execution_mode: ExecutionMode::Replicated,
        subnet_memory_saturation: ResourceSaturation::default(),
    }
}

fn make_network_topology(own_subnet_id: SubnetId, own_subnet_type: SubnetType) -> NetworkTopology {
    let routing_table = Arc::new(RoutingTable::try_from(btreemap! {
            CanisterIdRange{ start: CanisterId::from(0), end: CanisterId::from(0xff) } => own_subnet_id,
        }).unwrap());
    NetworkTopology {
        routing_table,
        subnets: btreemap! {
            own_subnet_id => SubnetTopology {
                subnet_type: own_subnet_type,
                ..SubnetTopology::default()
            }
        },
        ..NetworkTopology::default()
    }
}

// Not used in all test crates
#[allow(dead_code)]
pub fn default_network_topology() -> NetworkTopology {
    make_network_topology(subnet_test_id(1), SubnetType::Application)
}

pub struct ApiTypeBuilder;

// Note some methods of the builder might be unused in different test crates
#[allow(dead_code)]
#[allow(clippy::new_without_default)]
impl ApiTypeBuilder {
    pub fn build_update_api() -> ApiType {
        ApiType::update(
            mock_time(),
            vec![],
            Cycles::zero(),
            user_test_id(1).get(),
            CallContextId::from(1),
        )
    }

    pub fn build_system_task_api() -> ApiType {
        ApiType::system_task(
            IC_00.get(),
            SystemMethod::CanisterHeartbeat,
            mock_time(),
            CallContextId::from(1),
        )
    }

    pub fn build_reply_api(incoming_cycles: Cycles) -> ApiType {
        ApiType::reply_callback(
            mock_time(),
            PrincipalId::new_anonymous(),
            vec![],
            incoming_cycles,
            CallContextId::new(1),
            false,
            ExecutionMode::Replicated,
        )
    }

    pub fn build_reject_api(reject_context: RejectContext) -> ApiType {
        ApiType::reject_callback(
            mock_time(),
            PrincipalId::new_anonymous(),
            reject_context,
            Cycles::zero(),
            call_context_test_id(1),
            false,
            ExecutionMode::Replicated,
        )
    }
}

pub fn get_system_api(
    api_type: ApiType,
    system_state: &SystemState,
    cycles_account_manager: CyclesAccountManager,
) -> SystemApiImpl {
    let sandbox_safe_system_state = SandboxSafeSystemState::new(
        system_state,
        cycles_account_manager,
        &NetworkTopology::default(),
        SchedulerConfig::application_subnet().dirty_page_overhead,
        execution_parameters().compute_allocation,
    );
    SystemApiImpl::new(
        api_type,
        sandbox_safe_system_state,
        CANISTER_CURRENT_MEMORY_USAGE,
        execution_parameters(),
        SubnetAvailableMemory::new(
            SUBNET_MEMORY_CAPACITY,
            SUBNET_MEMORY_CAPACITY,
            SUBNET_MEMORY_CAPACITY,
        ),
        EmbeddersConfig::default()
            .feature_flags
            .wasm_native_stable_memory,
        EmbeddersConfig::default().max_sum_exported_function_name_lengths,
        Memory::new_for_testing(),
        Arc::new(DefaultOutOfInstructionsHandler {}),
        no_op_logger(),
    )
}

pub fn get_system_state() -> SystemState {
    let mut system_state = SystemStateBuilder::new().build();
    system_state
        .call_context_manager_mut()
        .unwrap()
        .new_call_context(
            CallOrigin::CanisterUpdate(canister_test_id(33), CallbackId::from(5)),
            Cycles::new(50),
            Time::from_nanos_since_unix_epoch(0),
        );
    system_state
}

pub fn get_cmc_system_state() -> SystemState {
    let mut system_state = get_system_state();
    system_state.canister_id = CYCLES_MINTING_CANISTER_ID;
    system_state
}
