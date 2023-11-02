use ic_base_types::{CanisterId, NumBytes, NumSeconds, PrincipalId, SubnetId};
use ic_btc_interface::Network;
use ic_btc_types_internal::{
    BitcoinAdapterResponse, BitcoinAdapterResponseWrapper, GetSuccessorsRequestInitial,
    GetSuccessorsResponseComplete,
};
use ic_ic00_types::{
    BitcoinGetSuccessorsResponse, CanisterChange, CanisterChangeDetails, CanisterChangeOrigin,
    Payload as _,
};
use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::replicated_state::testing::ReplicatedStateTesting;
use ic_replicated_state::testing::{CanisterQueuesTesting, SystemStateTesting};
use ic_replicated_state::{
    canister_state::execution_state::{CustomSection, CustomSectionType, WasmMetadata},
    metadata_state::subnet_call_context_manager::{BitcoinGetSuccessorsContext, SubnetCallContext},
    replicated_state::{MemoryTaken, PeekableOutputIterator, ReplicatedStateMessageRouting},
    CanisterState, IngressHistoryState, ReplicatedState, SchedulerState, StateError, SystemState,
};
use ic_test_utilities::mock_time;
use ic_test_utilities::state::{arb_replicated_state_with_queues, ExecutionStateBuilder};
use ic_test_utilities::types::ids::{canister_test_id, message_test_id, user_test_id, SUBNET_1};
use ic_test_utilities::types::messages::{RequestBuilder, ResponseBuilder};
use ic_types::ingress::{IngressState, IngressStatus};
use ic_types::{
    messages::{
        CanisterMessage, Payload, Request, RequestOrResponse, Response, MAX_RESPONSE_COUNT_BYTES,
    },
    CountBytes, Cycles, MemoryAllocation, Time,
};
use maplit::btreemap;
use proptest::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;
use std::sync::Arc;

const SUBNET_ID: SubnetId = SubnetId::new(PrincipalId::new(29, [0xfc; 29]));
const CANISTER_ID: CanisterId = CanisterId::from_u64(42);
const OTHER_CANISTER_ID: CanisterId = CanisterId::from_u64(13);
const SUBNET_AVAILABLE_MEMORY: i64 = i64::MAX / 2;

fn request_from(canister_id: CanisterId) -> RequestOrResponse {
    RequestBuilder::default()
        .sender(canister_id)
        .receiver(CANISTER_ID)
        .build()
        .into()
}

fn request_to(canister_id: CanisterId) -> Request {
    RequestBuilder::default()
        .sender(CANISTER_ID)
        .receiver(canister_id)
        .build()
}

fn response_from(canister_id: CanisterId) -> Response {
    ResponseBuilder::default()
        .respondent(canister_id)
        .originator(CANISTER_ID)
        .build()
}

fn response_to(canister_id: CanisterId) -> Response {
    ResponseBuilder::default()
        .respondent(CANISTER_ID)
        .originator(canister_id)
        .build()
}

/// Fixture using `SUBNET_ID` as its own subnet id and `CANISTER_ID` as the id
/// for the model canister used to send requests (responses) to and from.
/// Such messages are generated by the functions `request_from` et. al.
struct ReplicatedStateFixture {
    state: ReplicatedState,
}

impl ReplicatedStateFixture {
    fn new() -> ReplicatedStateFixture {
        Self::with_canisters(&[CANISTER_ID])
    }

    pub fn with_canisters(canister_ids: &[CanisterId]) -> ReplicatedStateFixture {
        Self::with_wasm_metadata(canister_ids, WasmMetadata::new(BTreeMap::new()))
    }

    pub fn with_wasm_metadata(
        canister_ids: &[CanisterId],
        wasm_metadata: WasmMetadata,
    ) -> ReplicatedStateFixture {
        let mut state = ReplicatedState::new(SUBNET_ID, SubnetType::Application);
        for canister_id in canister_ids {
            let scheduler_state = SchedulerState::default();
            let system_state = SystemState::new_running_for_testing(
                *canister_id,
                user_test_id(24).get(),
                Cycles::new(1 << 36),
                NumSeconds::from(100_000),
            );
            let execution_state = ExecutionStateBuilder::default()
                .with_wasm_metadata(wasm_metadata.clone())
                .build();
            state.put_canister_state(CanisterState::new(
                system_state,
                Some(execution_state),
                scheduler_state,
            ));
        }
        ReplicatedStateFixture { state }
    }

    fn push_input(
        &mut self,
        msg: RequestOrResponse,
    ) -> Result<(), (StateError, RequestOrResponse)> {
        self.state
            .push_input(msg, &mut SUBNET_AVAILABLE_MEMORY.clone())
    }

    fn pop_input(&mut self) -> Option<CanisterMessage> {
        self.state
            .canister_state_mut(&CANISTER_ID)
            .unwrap()
            .pop_input()
    }

    fn push_output_request(
        &mut self,
        request: Request,
        time: Time,
    ) -> Result<(), (StateError, Arc<Request>)> {
        self.state
            .canister_state_mut(&CANISTER_ID)
            .unwrap()
            .push_output_request(request.into(), time)
    }

    fn push_output_response(&mut self, response: Response) {
        self.state
            .canister_state_mut(&CANISTER_ID)
            .unwrap()
            .push_output_response(response.into());
    }

    fn push_to_streams(&mut self, msgs: Vec<RequestOrResponse>) {
        let mut streams = self.state.take_streams();
        for msg in msgs.into_iter() {
            streams.push(SUBNET_ID, msg);
        }
        self.state.put_streams(streams);
    }

    fn memory_taken(&self) -> MemoryTaken {
        self.state.memory_taken()
    }

    fn remote_subnet_input_schedule(&self, canister: &CanisterId) -> &VecDeque<CanisterId> {
        self.state
            .canister_state(canister)
            .unwrap()
            .system_state
            .queues()
            .get_remote_subnet_input_schedule()
    }

    fn local_subnet_input_schedule(&self, canister: &CanisterId) -> &VecDeque<CanisterId> {
        self.state
            .canister_state(canister)
            .unwrap()
            .system_state
            .queues()
            .get_local_subnet_input_schedule()
    }
}

fn assert_execution_memory_taken(total_memory_usage: usize, fixture: &ReplicatedStateFixture) {
    assert_eq!(
        total_memory_usage as u64,
        fixture.memory_taken().execution().get()
    );
}

fn assert_message_memory_taken(queues_memory_usage: usize, fixture: &ReplicatedStateFixture) {
    assert_eq!(
        queues_memory_usage as u64,
        fixture.memory_taken().messages().get()
    );
}

fn assert_canister_history_memory_taken(
    canister_history_memory_usage: usize,
    fixture: &ReplicatedStateFixture,
) {
    assert_eq!(
        canister_history_memory_usage as u64,
        fixture.memory_taken().canister_history().get(),
    );
}

fn assert_wasm_custom_sections_memory_taken(
    wasm_custom_sections_memory_usage: u64,
    fixture: &ReplicatedStateFixture,
) {
    assert_eq!(
        wasm_custom_sections_memory_usage,
        fixture.memory_taken().wasm_custom_sections().get()
    );
}

fn assert_subnet_available_memory(
    initial_available_memory: i64,
    queues_memory_usage: usize,
    actual: i64,
) {
    assert_eq!(
        initial_available_memory - queues_memory_usage as i64,
        actual
    );
}

#[test]
fn memory_taken_by_canister_queues() {
    let mut fixture = ReplicatedStateFixture::new();
    let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

    // Zero memory used initially.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(0, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a request into a canister input queue.
    fixture
        .state
        .push_input(
            request_from(OTHER_CANISTER_ID),
            &mut subnet_available_memory,
        )
        .unwrap();

    // Reserved memory for one response.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
    assert_subnet_available_memory(
        SUBNET_AVAILABLE_MEMORY,
        MAX_RESPONSE_COUNT_BYTES,
        subnet_available_memory,
    );

    // Pop input request.
    assert!(fixture.pop_input().is_some());

    // Unchanged memory usage.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a response into the output queue.
    let response = response_to(OTHER_CANISTER_ID);
    fixture.push_output_response(response.clone());

    // Memory used by response only.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(response.count_bytes(), &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
}

#[test]
fn memory_taken_by_subnet_queues() {
    let mut fixture = ReplicatedStateFixture::new();
    let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

    // Zero memory used initially.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(0, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a request into the subnet input queues.
    fixture
        .state
        .push_input(
            request_to(SUBNET_ID.into()).into(),
            &mut subnet_available_memory,
        )
        .unwrap();

    // Reserved memory for one response.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
    assert_subnet_available_memory(
        SUBNET_AVAILABLE_MEMORY,
        MAX_RESPONSE_COUNT_BYTES,
        subnet_available_memory,
    );

    // Pop subnet input request.
    assert!(fixture.state.pop_subnet_input().is_some());

    // Unchanged memory usage.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a response into the subnet output queues.
    let response = response_from(SUBNET_ID.into());
    fixture
        .state
        .push_subnet_output_response(response.clone().into());

    // Memory used by response only.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(response.count_bytes(), &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
}

#[test]
fn memory_taken_by_stream_responses() {
    let mut fixture = ReplicatedStateFixture::new();

    // Zero memory used initially.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(0, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a request and a response into a stream.
    let response = response_to(OTHER_CANISTER_ID);
    fixture.push_to_streams(vec![
        request_to(OTHER_CANISTER_ID).into(),
        response.clone().into(),
    ]);

    // Memory only used by response, not request.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(response.count_bytes(), &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
}

#[test]
fn memory_taken_by_wasm_custom_sections() {
    let mut custom_sections: BTreeMap<String, CustomSection> = BTreeMap::new();
    custom_sections.insert(
        String::from("candid"),
        CustomSection::new(CustomSectionType::Private, vec![0; 10 * 1024]),
    );
    let wasm_metadata = WasmMetadata::new(custom_sections);
    let wasm_metadata_memory = wasm_metadata.memory_usage();

    let mut fixture = ReplicatedStateFixture::with_wasm_metadata(&[CANISTER_ID], wasm_metadata);
    let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

    // Only memory for wasm custom sections is used initially.
    assert_execution_memory_taken(wasm_metadata_memory.get() as usize, &fixture);
    assert_wasm_custom_sections_memory_taken(wasm_metadata_memory.get(), &fixture);

    // Push a request into a canister input queue.
    fixture
        .state
        .push_input(
            request_from(OTHER_CANISTER_ID),
            &mut subnet_available_memory,
        )
        .unwrap();

    // Reserved memory for one response.
    assert_execution_memory_taken(wasm_metadata_memory.get() as usize, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(wasm_metadata_memory.get(), &fixture);
    assert_subnet_available_memory(
        SUBNET_AVAILABLE_MEMORY,
        MAX_RESPONSE_COUNT_BYTES,
        subnet_available_memory,
    );
}

#[test]
fn memory_taken_by_canister_history() {
    let mut fixture = ReplicatedStateFixture::with_wasm_metadata(
        &[CANISTER_ID],
        WasmMetadata::new(BTreeMap::new()),
    );

    // No memory is used initially.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(0, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Memory for two canister changes.
    let canister_history_memory: usize =
        size_of::<CanisterChange>() + (size_of::<CanisterChange>() + 4 * size_of::<PrincipalId>());

    // Push two canister changes into canister history.
    let canister_state = fixture.state.canister_state_mut(&CANISTER_ID).unwrap();
    canister_state.system_state.add_canister_change(
        Time::from_nanos_since_unix_epoch(0),
        CanisterChangeOrigin::from_user(user_test_id(42).get()),
        CanisterChangeDetails::canister_creation(vec![
            canister_test_id(777).get(),
            user_test_id(42).get(),
        ]),
    );
    canister_state.system_state.add_canister_change(
        Time::from_nanos_since_unix_epoch(16),
        CanisterChangeOrigin::from_user(user_test_id(123).get()),
        CanisterChangeDetails::controllers_change(vec![
            canister_test_id(0).get(),
            canister_test_id(1).get(),
        ]),
    );
    assert_execution_memory_taken(canister_history_memory, &fixture);
    assert_canister_history_memory_taken(canister_history_memory, &fixture);

    // Test fixed memory allocation.
    let canister_state = fixture.state.canister_state_mut(&CANISTER_ID).unwrap();
    canister_state.system_state.memory_allocation = MemoryAllocation::Reserved(NumBytes::from(888));
    assert_execution_memory_taken(888 + canister_history_memory, &fixture);
    assert_canister_history_memory_taken(canister_history_memory, &fixture);

    // Reset canister memory allocation.
    let canister_state = fixture.state.canister_state_mut(&CANISTER_ID).unwrap();
    canister_state.system_state.memory_allocation = MemoryAllocation::BestEffort;

    // Test a system subnet.
    fixture.state.metadata.own_subnet_type = SubnetType::System;

    assert_execution_memory_taken(canister_history_memory, &fixture);
    assert_canister_history_memory_taken(canister_history_memory, &fixture);
}

#[test]
fn push_subnet_queues_input_respects_subnet_available_memory() {
    let mut fixture = ReplicatedStateFixture::new();
    let initial_available_memory = MAX_RESPONSE_COUNT_BYTES as i64;
    let mut subnet_available_memory = initial_available_memory;

    // Zero memory used initially.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(0, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);

    // Push a request into the subnet input queues.
    fixture
        .state
        .push_input(
            request_to(SUBNET_ID.into()).into(),
            &mut subnet_available_memory,
        )
        .unwrap();

    // Reserved memory for one response.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
    assert_subnet_available_memory(
        initial_available_memory,
        MAX_RESPONSE_COUNT_BYTES,
        subnet_available_memory,
    );

    // Push a second request into the subnet input queues.
    let res = fixture.state.push_input(
        request_to(SUBNET_ID.into()).into(),
        &mut subnet_available_memory,
    );

    // No more memory for a second request.
    assert_eq!(
        Err((
            StateError::OutOfMemory {
                requested: (MAX_RESPONSE_COUNT_BYTES as u64).into(),
                available: 0.into()
            },
            request_to(SUBNET_ID.into()).into(),
        )),
        res
    );

    // Unchanged memory usage.
    assert_execution_memory_taken(0, &fixture);
    assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &fixture);
    assert_canister_history_memory_taken(0, &fixture);
    assert_wasm_custom_sections_memory_taken(0, &fixture);
    assert_eq!(0, subnet_available_memory);
}

#[test]
fn push_input_queues_respects_local_remote_subnet() {
    let mut fixture = ReplicatedStateFixture::new();

    // Assert the queues are empty.
    assert!(fixture.pop_input().is_none());
    assert!(fixture.state.canister_state(&OTHER_CANISTER_ID).is_none());

    // Push message from the remote canister, should be in the remote subnet
    // queue.
    fixture.push_input(request_from(OTHER_CANISTER_ID)).unwrap();
    assert_eq!(fixture.remote_subnet_input_schedule(&CANISTER_ID).len(), 1);

    // Push message from the local canister, should be in the local subnet queue.
    fixture.push_input(request_from(CANISTER_ID)).unwrap();
    assert_eq!(fixture.local_subnet_input_schedule(&CANISTER_ID).len(), 1);

    // Push message from the local subnet, should be in the local subnet queue.
    fixture
        .push_input(request_from(CanisterId::new(SUBNET_ID.get()).unwrap()))
        .unwrap();
    assert_eq!(fixture.local_subnet_input_schedule(&CANISTER_ID).len(), 2);
}

#[test]
fn insert_bitcoin_response_non_matching() {
    let mut state = ReplicatedState::new(SUBNET_ID, SubnetType::Application);

    assert_eq!(
        state.push_response_bitcoin(BitcoinAdapterResponse {
            response: BitcoinAdapterResponseWrapper::GetSuccessorsResponse(
                GetSuccessorsResponseComplete {
                    blocks: vec![],
                    next: vec![],
                },
            ),
            callback_id: 0,
        }),
        Err(StateError::BitcoinNonMatchingResponse { callback_id: 0 })
    );
}

#[test]
fn insert_bitcoin_response() {
    let mut state = ReplicatedState::new(SUBNET_ID, SubnetType::Application);

    state.metadata.subnet_call_context_manager.push_context(
        SubnetCallContext::BitcoinGetSuccessors(BitcoinGetSuccessorsContext {
            request: RequestBuilder::default().build(),
            payload: GetSuccessorsRequestInitial {
                network: Network::Regtest,
                anchor: vec![],
                processed_block_hashes: vec![],
            },
            time: mock_time(),
        }),
    );

    let response = GetSuccessorsResponseComplete {
        blocks: vec![],
        next: vec![],
    };

    state
        .push_response_bitcoin(BitcoinAdapterResponse {
            response: BitcoinAdapterResponseWrapper::GetSuccessorsResponse(response.clone()),
            callback_id: 0,
        })
        .unwrap();

    assert_eq!(
        state.consensus_queue[0].response_payload,
        Payload::Data(BitcoinGetSuccessorsResponse::Complete(response).encode())
    );
}

#[test]
fn time_out_requests_updates_subnet_input_schedules_correctly() {
    let mut fixture = ReplicatedStateFixture::with_canisters(&[CANISTER_ID, OTHER_CANISTER_ID]);

    // Push 3 requests into the canister with id `local_canister_id1`:
    // - one to self.
    // - one to a another local canister.
    // - one to a remote canister.
    let remote_canister_id = CanisterId::from_u64(123);
    for receiver in [CANISTER_ID, OTHER_CANISTER_ID, remote_canister_id] {
        fixture
            .push_output_request(request_to(receiver), mock_time())
            .unwrap();
    }

    // Time out everything, then check that subnet input schedules are as expected.
    fixture.state.metadata.batch_time = Time::from_nanos_since_unix_epoch(u64::MAX);
    assert_eq!(3, fixture.state.time_out_requests());

    assert_eq!(2, fixture.local_subnet_input_schedule(&CANISTER_ID).len());
    for canister_id in [CANISTER_ID, OTHER_CANISTER_ID] {
        assert!(fixture
            .local_subnet_input_schedule(&CANISTER_ID)
            .contains(&canister_id));
    }
    assert_eq!(
        fixture.remote_subnet_input_schedule(&CANISTER_ID),
        &VecDeque::from(vec![remote_canister_id])
    );
}

#[test]
fn split() {
    // We will be splitting subnet A into A' and B.
    const SUBNET_A: SubnetId = SUBNET_ID;
    const SUBNET_B: SubnetId = SUBNET_1;

    const CANISTER_1: CanisterId = CANISTER_ID;
    const CANISTER_2: CanisterId = OTHER_CANISTER_ID;
    const CANISTERS: [CanisterId; 2] = [CANISTER_1, CANISTER_2];

    // Retain `CANISTER_1` on `SUBNET_A`, migrate `CANISTER_2` to `SUBNET_B`.
    let routing_table = RoutingTable::try_from(btreemap! {
        CanisterIdRange {start: CANISTER_1, end: CANISTER_1} => SUBNET_A,
        CanisterIdRange {start: CANISTER_2, end: CANISTER_2} => SUBNET_B,
    })
    .unwrap();

    // Fixture with 2 canisters.
    let mut fixture = ReplicatedStateFixture::with_canisters(&CANISTERS);

    // Stream with a couple of requests. The details don't matter, should be
    // retained unmodified on subnet A' only.
    fixture.push_to_streams(vec![
        request_to(CANISTER_1).into(),
        request_to(CANISTER_2).into(),
    ]);

    // Makes an `IngressHistoryState` with one `Received` message addressed to each
    // of `canisters`.
    let make_ingress_history = |canisters: &[CanisterId]| {
        let mut ingress_history = IngressHistoryState::default();
        for (i, canister) in CANISTERS.iter().enumerate() {
            if canisters.contains(canister) {
                ingress_history.insert(
                    message_test_id(i as u64),
                    IngressStatus::Known {
                        receiver: canister.get(),
                        user_id: user_test_id(i as u64),
                        time: mock_time(),
                        state: IngressState::Received,
                    },
                    mock_time(),
                    NumBytes::from(u64::MAX),
                );
            }
        }
        ingress_history
    };
    // Ingress history: 2 `Received` messages, addressed to canisters 1 and 2.
    // Should be retained on both sides after phase 1, split after phase 2.
    fixture.state.metadata.ingress_history = make_ingress_history(&CANISTERS);

    // Subnet queues. Should be preserved on subnet A' only.
    fixture
        .push_input(
            RequestBuilder::default()
                .sender(CANISTER_1)
                .receiver(SUBNET_A.into())
                .build()
                .into(),
        )
        .unwrap();

    // Set up input schedules. Add a couple of input messages to each canister.
    for sender in CANISTERS {
        for receiver in CANISTERS {
            fixture
                .push_input(
                    RequestBuilder::default()
                        .sender(sender)
                        .receiver(receiver)
                        .build()
                        .into(),
                )
                .unwrap();
        }
    }
    for canister in CANISTERS {
        assert_eq!(2, fixture.local_subnet_input_schedule(&canister).len());
        assert_eq!(0, fixture.remote_subnet_input_schedule(&canister).len());
    }

    //
    // Split off subnet A', phase 1.
    //
    let mut state_a = fixture
        .state
        .clone()
        .split(SUBNET_A, &routing_table, None)
        .unwrap();

    // Start off with the original state.
    let mut expected = fixture.state.clone();
    // Only `CANISTER_1` should be left.
    expected.canister_states.remove(&CANISTER_2);
    // And the split marker should be set.
    expected.metadata.split_from = Some(SUBNET_A);
    // Otherwise, the state should be the same.
    assert_eq!(expected, state_a);

    //
    // Subnet A', phase 2.
    //
    state_a.after_split();

    // Ingress history should only contain the message to `CANISTER_1`.
    expected.metadata.ingress_history = make_ingress_history(&[CANISTER_1]);
    // The input schedules of `CANISTER_1` should have been repartitioned.
    let mut canister_state = expected.canister_states.remove(&CANISTER_1).unwrap();
    canister_state
        .system_state
        .split_input_schedules(&CANISTER_1, &expected.canister_states);
    expected.canister_states.insert(CANISTER_1, canister_state);
    // And the split marker should be reset.
    expected.metadata.split_from = None;
    // Everything else should be the same as in phase 1.
    assert_eq!(expected, state_a);

    //
    // Split off subnet B, phase 1.
    //
    let mut state_b = fixture
        .state
        .clone()
        .split(SUBNET_B, &routing_table, None)
        .unwrap();

    // Subnet B state is based off of an empty state.
    let mut expected = ReplicatedState::new(SUBNET_B, fixture.state.metadata.own_subnet_type);
    // Only `CANISTER_2` should be left.
    expected.canister_states.insert(
        CANISTER_2,
        fixture.state.canister_state(&CANISTER_2).unwrap().clone(),
    );
    // The full ingress history should be preserved.
    expected.metadata.ingress_history = fixture.state.metadata.ingress_history;
    // And the split marker should be set.
    expected.metadata.split_from = Some(SUBNET_A);
    // Otherwise, the state should be the same.
    assert_eq!(expected, state_b);

    //
    // Subnet B, phase 2.
    //
    state_b.after_split();

    // Ingress history should only contain the message to `CANISTER_2`.
    expected.metadata.ingress_history = make_ingress_history(&[CANISTER_2]);
    // The input schedules of `CANISTER_2` should have been repartitioned.
    let mut canister_state = expected.canister_states.remove(&CANISTER_2).unwrap();
    canister_state
        .system_state
        .split_input_schedules(&CANISTER_2, &expected.canister_states);
    expected.canister_states.insert(CANISTER_2, canister_state);
    // And the split marker should be reset.
    expected.metadata.split_from = None;
    // Everything else should be the same as in phase 1.
    assert_eq!(expected, state_b);
}

proptest! {
    #[test]
    fn peek_and_next_consistent(
        (mut replicated_state, _, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8))
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut num_requests = 0;
        while let Some((queue_id, msg)) = output_iter.peek() {
            num_requests += 1;
            assert_eq!(Some((queue_id, msg.clone())), output_iter.next());
        }

        drop(output_iter);
        assert_eq!(total_requests, num_requests);
        assert_eq!(replicated_state.output_message_count(), 0);
    }

    /// Replicated state with multiple canisters, each with multiple output queues
    /// of size 1. Some messages are consumed, some (size 1) queues are excluded.
    ///
    /// Expect consumed + excluded to equal initial size. Expect the messages in
    /// excluded queues to be left in the state.
    #[test]
    fn peek_and_next_consistent_with_ignore(
        (mut replicated_state, _, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, None),
        start in 0..=1,
        exclude_step in 2..=5,
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut i = start;
        let mut excluded = 0;
        let mut consumed = 0;
        while let Some((queue_id, msg)) = output_iter.peek() {
            i += 1;
            if i % exclude_step == 0 {
                output_iter.exclude_queue();
                excluded += 1;
            } else {
                assert_eq!(Some((queue_id, msg.clone())), output_iter.next());
                consumed += 1;
            }
        }

        drop(output_iter);
        assert_eq!(total_requests, excluded + consumed);
        assert_eq!(replicated_state.output_message_count(), excluded);
    }

    #[test]
    fn iter_yields_correct_elements(
       (mut replicated_state, mut raw_requests, _total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, None),
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        for (_, msg) in &mut output_iter {
            let mut requests = raw_requests.pop_front().unwrap();
            while requests.is_empty() {
                requests = raw_requests.pop_front().unwrap();
            }

            if let Some(raw_msg) = requests.pop_front() {
                assert_eq!(msg, raw_msg, "Popped message does not correspond with expected message. popped: {:?}. expected: {:?}.", msg, raw_msg);
            } else {
                panic!("Pop yielded an element that was not contained in the respective queue");
            }

            raw_requests.push_back(requests);
        }

        drop(output_iter);
        // Ensure that actually all elements have been consumed.
        assert_eq!(raw_requests.iter().map(|requests| requests.len()).sum::<usize>(), 0);
        assert_eq!(replicated_state.output_message_count(), 0);
    }

    #[test]
    fn iter_with_ignore_yields_correct_elements(
       (mut replicated_state, mut raw_requests, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 10, 10, None),
        start in 0..=1,
        ignore_step in 2..=5,
    ) {
        let mut consumed = 0;
        let mut ignored_requests = Vec::new();
        // Check whether popping elements with ignores in between yields the expected messages
        {
            let mut output_iter = replicated_state.output_into_iter();

            let mut i = start;
            while let Some((_, msg)) = output_iter.peek() {

                let mut requests = raw_requests.pop_front().unwrap();
                while requests.is_empty() {
                    requests = raw_requests.pop_front().unwrap();
                }

                i += 1;
                if i % ignore_step == 0 {
                    // Popping the front of the requests will amount to the same as ignoring as
                    // we use queues of size one in this test.
                    let popped = requests.pop_front().unwrap();
                    assert_eq!(*msg, popped);
                    output_iter.exclude_queue();
                    ignored_requests.push(popped);
                    // We push the queue to the front as the canister gets another chance if one
                    // of its queues are ignored in the current implementation.
                    raw_requests.push_front(requests);
                    continue;
                }

                let (_, msg) = output_iter.next().unwrap();
                if let Some(raw_msg) = requests.pop_front() {
                    consumed += 1;
                    assert_eq!(msg, raw_msg, "Popped message does not correspond with expected message. popped: {:?}. expected: {:?}.", msg, raw_msg);
                } else {
                    panic!("Pop yielded an element that was not contained in the respective queue");
                }

                raw_requests.push_back(requests);
            }
        }

        let remaining_output = replicated_state.output_message_count();

        assert_eq!(remaining_output, total_requests - consumed);
        assert_eq!(remaining_output, ignored_requests.len());

        for raw in ignored_requests {
            let queues = if let Some(canister) = replicated_state.canister_states.get_mut(&raw.sender()) {
                canister.system_state.queues_mut()
            } else {
                replicated_state.subnet_queues_mut()
            };

            let msg = queues.pop_canister_output(&raw.receiver()).unwrap();
            assert_eq!(raw, msg);
        }

        assert_eq!(replicated_state.output_message_count(), 0);

    }

    #[test]
    fn peek_next_loop_terminates(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        while output_iter.peek().is_some() {
            output_iter.next();
        }
    }

    #[test]
    fn ignore_leaves_state_untouched(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
    ) {
        let expected_state = replicated_state.clone();
        {
            let mut output_iter = replicated_state.output_into_iter();

            while output_iter.peek().is_some() {
                output_iter.exclude_queue();
            }
        }

        assert_eq!(expected_state, replicated_state);
    }

    #[test]
    fn peek_next_loop_with_ignores_terminates(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
        start in 0..=1,
        ignore_step in 2..=5,
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut i = start;
        while output_iter.peek().is_some() {
            i += 1;
            if i % ignore_step == 0 {
                output_iter.exclude_queue();
                continue;
            }
            output_iter.next();
        }
    }
}
