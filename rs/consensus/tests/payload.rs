mod framework;

use crate::framework::ConsensusDriver;
use ic_artifact_pool::{consensus_pool, dkg_pool, ecdsa_pool};
use ic_consensus::consensus::dkg_key_manager::DkgKeyManager;
use ic_consensus::{certification::CertifierImpl, dkg};
use ic_consensus_utils::{membership::Membership, pool_reader::PoolReader};
use ic_https_outcalls_consensus::test_utils::FakeCanisterHttpPayloadBuilder;
use ic_interfaces_state_manager::Labeled;
use ic_interfaces_state_manager_mocks::MockStateManager;
use ic_logger::replica_logger::no_op_logger;
use ic_metrics::MetricsRegistry;
use ic_test_utilities::consensus::batch::MockBatchPayloadBuilder;
use ic_test_utilities::{
    consensus::make_genesis,
    crypto::CryptoReturningOk,
    ingress_selector::FakeIngressSelector,
    message_routing::FakeMessageRouting,
    self_validating_payload_builder::FakeSelfValidatingPayloadBuilder,
    state::get_initial_state,
    types::ids::{node_test_id, subnet_test_id},
    types::messages::SignedIngressBuilder,
    xnet_payload_builder::FakeXNetPayloadBuilder,
    FastForwardTimeSource,
};
use ic_test_utilities_registry::{
    setup_registry, FakeLocalStoreCertifiedTimeReader, SubnetRecordBuilder,
};
use ic_types::{
    crypto::CryptoHash, malicious_flags::MaliciousFlags, replica_config::ReplicaConfig,
    CryptoHashOfState, Height,
};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Test that the batches that Consensus produces contain expected batch
/// numbers and payloads
#[test]
fn consensus_produces_expected_batches() {
    ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
        let ingress0 = SignedIngressBuilder::new().nonce(0).build();
        let ingress1 = SignedIngressBuilder::new().nonce(1).build();
        let ingress_selector = FakeIngressSelector::new();
        ingress_selector.enqueue(vec![ingress0.clone()]);
        ingress_selector.enqueue(vec![ingress1.clone()]);
        let ingress_selector = Arc::new(ingress_selector);

        let xnet_payload_builder = FakeXNetPayloadBuilder::new();
        let xnet_payload_builder = Arc::new(xnet_payload_builder);

        let self_validating_payload_builder = FakeSelfValidatingPayloadBuilder::new();
        let self_validating_payload_builder = Arc::new(self_validating_payload_builder);

        let canister_http_payload_builder = FakeCanisterHttpPayloadBuilder::new();
        let canister_http_payload_builder = Arc::new(canister_http_payload_builder);

        let query_stats_payload_builder = MockBatchPayloadBuilder::new().expect_noop();
        let query_stats_payload_builder = Arc::new(query_stats_payload_builder);

        let mut state_manager = MockStateManager::new();
        state_manager.expect_remove_states_below().return_const(());
        state_manager
            .expect_list_state_hashes_to_certify()
            .return_const(vec![]);
        state_manager
            .expect_latest_certified_height()
            .return_const(Height::new(0));
        state_manager
            .expect_latest_state_height()
            .return_const(Height::from(0));
        state_manager
            .expect_get_state_hash_at()
            .return_const(Ok(CryptoHashOfState::from(CryptoHash(vec![]))));
        state_manager
            .expect_get_state_at()
            .return_const(Ok(Labeled::new(
                Height::new(0),
                Arc::new(get_initial_state(0, 0)),
            )));
        let state_manager = Arc::new(state_manager);

        let router = FakeMessageRouting::default();
        *router.next_batch_height.write().unwrap() = Height::from(1); // skip genesis block

        let router = Arc::new(router);
        let node_id = node_test_id(0);
        let subnet_id = subnet_test_id(0);
        let replica_config = ReplicaConfig { node_id, subnet_id };
        let fake_crypto = CryptoReturningOk::default();
        let fake_crypto = Arc::new(fake_crypto);
        let metrics_registry = MetricsRegistry::new();
        let time = FastForwardTimeSource::new();
        let dkg_pool = Arc::new(RwLock::new(dkg_pool::DkgPoolImpl::new(
            metrics_registry.clone(),
            no_op_logger(),
        )));
        let ecdsa_pool = Arc::new(RwLock::new(ecdsa_pool::EcdsaPoolImpl::new(
            pool_config.clone(),
            no_op_logger(),
            metrics_registry.clone(),
        )));

        let registry_client = setup_registry(
            replica_config.subnet_id,
            vec![(1, SubnetRecordBuilder::from(&[node_test_id(0)]).build())],
        );
        let summary = dkg::make_genesis_summary(&*registry_client, replica_config.subnet_id, None);
        let consensus_pool = Arc::new(RwLock::new(consensus_pool::ConsensusPoolImpl::new(
            node_id,
            subnet_id,
            (&make_genesis(summary)).into(),
            pool_config.clone(),
            MetricsRegistry::new(),
            no_op_logger(),
        )));
        let consensus_cache = consensus_pool.read().unwrap().get_cache();
        let membership = Membership::new(
            consensus_cache.clone(),
            registry_client.clone(),
            replica_config.subnet_id,
        );
        let membership = Arc::new(membership);
        let dkg_key_manager = Arc::new(Mutex::new(DkgKeyManager::new(
            metrics_registry.clone(),
            Arc::clone(&fake_crypto) as Arc<_>,
            no_op_logger(),
            &PoolReader::new(&*consensus_pool.read().unwrap()),
        )));
        let fake_local_store_certified_time_reader =
            Arc::new(FakeLocalStoreCertifiedTimeReader::new(time.clone()));

        let (consensus, consensus_gossip) = ic_consensus::consensus::setup(
            replica_config.clone(),
            Arc::clone(&registry_client) as Arc<_>,
            Arc::clone(&membership) as Arc<_>,
            Arc::clone(&fake_crypto) as Arc<_>,
            Arc::clone(&ingress_selector) as Arc<_>,
            Arc::clone(&xnet_payload_builder) as Arc<_>,
            Arc::clone(&self_validating_payload_builder) as Arc<_>,
            Arc::clone(&canister_http_payload_builder) as Arc<_>,
            query_stats_payload_builder,
            Arc::clone(&dkg_pool) as Arc<_>,
            Arc::clone(&ecdsa_pool) as Arc<_>,
            dkg_key_manager.clone(),
            Arc::clone(&router) as Arc<_>,
            Arc::clone(&state_manager) as Arc<_>,
            Arc::clone(&time) as Arc<_>,
            MaliciousFlags::default(),
            metrics_registry.clone(),
            no_op_logger(),
            fake_local_store_certified_time_reader,
            0,
        );
        let dkg = dkg::DkgImpl::new(
            replica_config.node_id,
            Arc::clone(&fake_crypto) as Arc<_>,
            Arc::clone(&consensus_cache),
            dkg_key_manager,
            metrics_registry.clone(),
            no_op_logger(),
        );
        let certifier = CertifierImpl::new(
            replica_config.clone(),
            Arc::clone(&membership) as Arc<_>,
            Arc::clone(&fake_crypto) as Arc<_>,
            Arc::clone(&state_manager) as Arc<_>,
            Arc::clone(&consensus_cache),
            metrics_registry.clone(),
            no_op_logger(),
        );

        let driver = ConsensusDriver::new(
            replica_config.node_id,
            pool_config,
            Box::new(consensus),
            consensus_gossip,
            dkg,
            Box::new(certifier),
            consensus_pool,
            dkg_pool,
            no_op_logger(),
            metrics_registry,
        );
        driver.step(); // this stops before notary timeout expires after making 1st block
        time.advance_time(Duration::from_millis(2000));
        driver.step(); // this stops before notary timeout expires after making 2nd block
        time.advance_time(Duration::from_millis(2000));
        driver.step(); // this stops before notary timeout expires after making 3rd block
        let batches = router.batches.read().unwrap().clone();
        *router.batches.write().unwrap() = Vec::new();
        assert_eq!(batches.len(), 2);
        assert_ne!(batches[0].batch_number, batches[1].batch_number);
        let mut msgs: Vec<_> = batches[0].messages.signed_ingress_msgs.clone();
        assert_eq!(msgs.pop(), Some(ingress0));
        let mut msgs: Vec<_> = batches[1].messages.signed_ingress_msgs.clone();
        assert_eq!(msgs.pop(), Some(ingress1));
    })
}
