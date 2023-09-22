//! The P2P module exposes the peer-to-peer functionality.
//!
//! Specifically, it constructs all the artifact pools and the Consensus/P2P
//! time source.

use crossbeam_channel::{bounded, Sender};
use either::Either;
use ic_artifact_manager::{manager, *};
use ic_artifact_pool::{
    canister_http_pool::CanisterHttpPoolImpl,
    certification_pool::CertificationPoolImpl,
    consensus_pool::ConsensusPoolImpl,
    dkg_pool::DkgPoolImpl,
    ecdsa_pool::EcdsaPoolImpl,
    ingress_pool::{IngressPoolImpl, IngressPrioritizer},
};
use ic_config::{artifact_pool::ArtifactPoolConfig, transport::TransportConfig};
use ic_consensus::{
    certification::{setup as certification_setup, CertificationCrypto},
    consensus::{dkg_key_manager::DkgKeyManager, setup as consensus_setup},
    dkg, ecdsa,
};
use ic_consensus_utils::{
    crypto::ConsensusCrypto, membership::Membership, pool_reader::PoolReader,
};
use ic_crypto_tls_interfaces::{TlsConfig, TlsHandshake};
use ic_cycles_account_manager::CyclesAccountManager;
use ic_https_outcalls_consensus::{
    gossip::CanisterHttpGossipImpl, payload_builder::CanisterHttpPayloadBuilderImpl,
    pool_manager::CanisterHttpPoolManagerImpl,
};
use ic_icos_sev::Sev;
use ic_ingress_manager::IngressManager;
use ic_interfaces::{
    artifact_manager::{ArtifactClient, ArtifactProcessor, JoinGuard},
    artifact_pool::UnvalidatedArtifact,
    batch_payload::BatchPayloadBuilder,
    crypto::IngressSigVerifier,
    execution_environment::IngressHistoryReader,
    messaging::{MessageRouting, XNetPayloadBuilder},
    self_validating_payload::SelfValidatingPayloadBuilder,
    time_source::SysTimeSource,
};
use ic_interfaces_adapter_client::NonBlockingChannel;
use ic_interfaces_registry::{LocalStoreCertifiedTimeReader, RegistryClient};
use ic_interfaces_state_manager::{StateManager, StateReader};
use ic_interfaces_transport::Transport;
use ic_logger::{info, replica_logger::ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_p2p::{start_p2p, MAX_ADVERT_BUFFER};
use ic_quic_transport::DummyUdpSocket;
use ic_registry_client_helpers::subnet::SubnetRegistry;
use ic_replicated_state::ReplicatedState;
use ic_state_manager::state_sync::{StateSync, StateSyncArtifact};
use ic_transport::transport::create_transport;
use ic_types::{
    artifact::{Advert, ArtifactKind, ArtifactTag, FileTreeSyncAttribute},
    artifact_kind::{
        CanisterHttpArtifact, CertificationArtifact, ConsensusArtifact, DkgArtifact, EcdsaArtifact,
        IngressArtifact,
    },
    canister_http::{CanisterHttpRequest, CanisterHttpResponse},
    consensus::CatchUpPackage,
    consensus::HasHeight,
    crypto::CryptoHash,
    filetree_sync::{FileTreeSyncArtifact, FileTreeSyncId},
    malicious_flags::MaliciousFlags,
    messages::SignedIngress,
    p2p::GossipAdvert,
    replica_config::ReplicaConfig,
    NodeId, SubnetId,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
};

/// The P2P state sync client.
pub enum P2PStateSyncClient {
    /// The main client variant.
    Client(StateSync),
    /// The test client variant.
    TestClient(),
    /// The test chunking pool variant.
    TestChunkingPool(
        Box<dyn ArtifactClient<TestArtifact>>,
        Box<dyn ArtifactProcessor<TestArtifact> + Sync + 'static>,
    ),
}

/// The collection of all artifact pools.
struct ArtifactPools {
    ingress_pool: Arc<RwLock<IngressPoolImpl>>,
    certification_pool: Arc<RwLock<CertificationPoolImpl>>,
    dkg_pool: Arc<RwLock<DkgPoolImpl>>,
    ecdsa_pool: Arc<RwLock<EcdsaPoolImpl>>,
    canister_http_pool: Arc<RwLock<CanisterHttpPoolImpl>>,
}

struct P2PClients {
    consensus: ArtifactClientHandle<ConsensusArtifact>,
    ingress: ArtifactClientHandle<IngressArtifact>,
    certification: ArtifactClientHandle<CertificationArtifact>,
    dkg: ArtifactClientHandle<DkgArtifact>,
    ecdsa: ArtifactClientHandle<EcdsaArtifact>,
    https_outcalls: ArtifactClientHandle<CanisterHttpArtifact>,
}

pub type CanisterHttpAdapterClient =
    Box<dyn NonBlockingChannel<CanisterHttpRequest, Response = CanisterHttpResponse> + Send>;

/// The function constructs a P2P instance. Currently, it constructs all the
/// artifact pools and the Consensus/P2P time source. Artifact
/// clients are constructed and run in their separate actors.
#[allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::new_ret_no_self
)]
pub fn setup_consensus_and_p2p(
    log: &ReplicaLogger,
    metrics_registry: &MetricsRegistry,
    rt_handle: &tokio::runtime::Handle,
    artifact_pool_config: ArtifactPoolConfig,
    transport_config: TransportConfig,
    malicious_flags: MaliciousFlags,
    node_id: NodeId,
    subnet_id: SubnetId,
    // For testing purposes the caller can pass a transport object instead. Otherwise, the callee
    // constructs it from the 'transport_config'.
    transport: Option<Arc<dyn Transport>>,
    tls_config: Arc<dyn TlsConfig + Send + Sync>,
    tls_handshake: Arc<dyn TlsHandshake + Send + Sync>,
    state_manager: Arc<dyn StateManager<State = ReplicatedState>>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    consensus_pool: Arc<RwLock<ConsensusPoolImpl>>,
    catch_up_package: CatchUpPackage,
    state_sync_client: P2PStateSyncClient,
    xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
    self_validating_payload_builder: Arc<dyn SelfValidatingPayloadBuilder>,
    query_stats_payload_builder: Box<dyn BatchPayloadBuilder>,
    message_router: Arc<dyn MessageRouting>,
    consensus_crypto: Arc<dyn ConsensusCrypto + Send + Sync>,
    certifier_crypto: Arc<dyn CertificationCrypto + Send + Sync>,
    ingress_sig_crypto: Arc<dyn IngressSigVerifier + Send + Sync>,
    registry_client: Arc<dyn RegistryClient>,
    ingress_history_reader: Box<dyn IngressHistoryReader>,
    cycles_account_manager: Arc<CyclesAccountManager>,
    local_store_time_reader: Arc<dyn LocalStoreCertifiedTimeReader>,
    canister_http_adapter_client: CanisterHttpAdapterClient,
    registry_poll_delay_duration_ms: u64,
    time_source: Arc<SysTimeSource>,
) -> (
    Arc<RwLock<IngressPoolImpl>>,
    Sender<UnvalidatedArtifact<SignedIngress>>,
    Vec<Box<dyn JoinGuard>>,
) {
    let consensus_pool_cache = consensus_pool.read().unwrap().get_cache();
    let (advert_tx, advert_rx) = bounded(MAX_ADVERT_BUFFER);

    let (p2p_clients, mut join_handles, ingress_pool) = start_consensus(
        log,
        metrics_registry,
        node_id,
        subnet_id,
        artifact_pool_config,
        catch_up_package,
        Arc::clone(&consensus_crypto) as Arc<_>,
        Arc::clone(&certifier_crypto) as Arc<_>,
        Arc::clone(&ingress_sig_crypto) as Arc<_>,
        Arc::clone(&registry_client),
        state_manager,
        state_reader,
        xnet_payload_builder,
        self_validating_payload_builder,
        query_stats_payload_builder,
        message_router,
        ingress_history_reader,
        consensus_pool,
        malicious_flags,
        cycles_account_manager,
        local_store_time_reader,
        registry_poll_delay_duration_ms,
        advert_tx.clone(),
        canister_http_adapter_client,
        time_source.clone(),
    );
    let mut ingress_sender = p2p_clients.ingress.sender.clone();
    // P2P stack follows

    let mut backends: HashMap<ArtifactTag, Box<dyn manager::ArtifactManagerBackend>> =
        HashMap::new();
    backends.insert(
        CertificationArtifact::TAG,
        Box::new(p2p_clients.certification),
    );
    backends.insert(ConsensusArtifact::TAG, Box::new(p2p_clients.consensus));
    backends.insert(DkgArtifact::TAG, Box::new(p2p_clients.dkg));
    backends.insert(IngressArtifact::TAG, Box::new(p2p_clients.ingress));
    backends.insert(EcdsaArtifact::TAG, Box::new(p2p_clients.ecdsa));
    backends.insert(
        CanisterHttpArtifact::TAG,
        Box::new(p2p_clients.https_outcalls),
    );

    // StateSync
    match state_sync_client {
        P2PStateSyncClient::TestChunkingPool(pool_reader, client_on_state_change) => {
            std::mem::take(&mut backends);
            std::mem::take(&mut join_handles);
            let (ingress_tx, _r) = crossbeam_channel::unbounded();
            ingress_sender = ingress_tx;
            let (jh, sender) = run_artifact_processor(
                Arc::clone(&time_source) as Arc<_>,
                metrics_registry.clone(),
                client_on_state_change,
                move |req| {
                    let _ = advert_tx.send(req.into());
                },
            );
            join_handles.push(jh);
            backends.insert(
                TestArtifact::TAG,
                Box::new(ArtifactClientHandle {
                    pool_reader,
                    sender,
                    time_source,
                }),
            );
        }
        P2PStateSyncClient::Client(client) => {
            let advert_tx = advert_tx.clone();
            let (jh, sender) = run_artifact_processor(
                Arc::clone(&time_source) as Arc<_>,
                metrics_registry.clone(),
                Box::new(client.clone()) as Box<_>,
                move |req| {
                    let _ = advert_tx.send(req.into());
                },
            );
            join_handles.push(jh);
            backends.insert(
                StateSyncArtifact::TAG,
                Box::new(ArtifactClientHandle {
                    pool_reader: Box::new(client),
                    sender,
                    time_source: time_source.clone(),
                }),
            );
        }
        P2PStateSyncClient::TestClient() => (),
    }

    let sev_handshake = Arc::new(Sev::new(node_id, registry_client.clone()));

    // Quic transport
    let (_, topology_watcher) = ic_peer_manager::start_peer_manager(
        log.clone(),
        metrics_registry,
        rt_handle,
        subnet_id,
        consensus_pool_cache.clone(),
        registry_client.clone(),
    );

    let transport_addr: SocketAddr = (
        IpAddr::from_str(&transport_config.node_ip).expect("Invalid IP"),
        transport_config.listening_port,
    )
        .into();
    let _quic_transport = Arc::new(ic_quic_transport::QuicTransport::build(
        log,
        metrics_registry,
        rt_handle.clone(),
        tls_config,
        registry_client.clone(),
        sev_handshake.clone(),
        node_id,
        topology_watcher,
        Either::<_, DummyUdpSocket>::Left(transport_addr),
        None,
    ));

    // Tcp transport
    let oldest_registry_version_in_use = consensus_pool_cache.get_oldest_registry_version_in_use();
    let transport = transport.unwrap_or_else(|| {
        create_transport(
            node_id,
            transport_config.clone(),
            registry_client.get_latest_version(),
            oldest_registry_version_in_use,
            metrics_registry.clone(),
            tls_handshake,
            sev_handshake,
            rt_handle.clone(),
            log.clone(),
            false,
        )
    });

    let artifact_manager = Arc::new(manager::ArtifactManagerImpl::new_with_default_priority_fn(
        backends,
    ));

    join_handles.push(start_p2p(
        log,
        metrics_registry,
        rt_handle,
        node_id,
        subnet_id,
        transport_config,
        registry_client,
        transport,
        consensus_pool_cache,
        artifact_manager,
        advert_rx,
    ));
    (ingress_pool, ingress_sender, join_handles)
}

/// The function creates the Consensus stack (including all Consensus clients)
/// and starts the artifact manager event loop for each client.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn start_consensus(
    log: &ReplicaLogger,
    metrics_registry: &MetricsRegistry,
    node_id: NodeId,
    subnet_id: SubnetId,
    artifact_pool_config: ArtifactPoolConfig,
    catch_up_package: CatchUpPackage,
    // ConsensusCrypto is an extension of the Crypto trait and we can
    // not downcast traits.
    consensus_crypto: Arc<dyn ConsensusCrypto>,
    certifier_crypto: Arc<dyn CertificationCrypto>,
    ingress_sig_crypto: Arc<dyn IngressSigVerifier + Send + Sync>,
    registry_client: Arc<dyn RegistryClient>,
    state_manager: Arc<dyn StateManager<State = ReplicatedState>>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
    self_validating_payload_builder: Arc<dyn SelfValidatingPayloadBuilder>,
    query_stats_payload_builder: Box<dyn BatchPayloadBuilder>,
    message_router: Arc<dyn MessageRouting>,
    ingress_history_reader: Box<dyn IngressHistoryReader>,
    consensus_pool: Arc<RwLock<ConsensusPoolImpl>>,
    malicious_flags: MaliciousFlags,
    cycles_account_manager: Arc<CyclesAccountManager>,
    local_store_time_reader: Arc<dyn LocalStoreCertifiedTimeReader>,
    registry_poll_delay_duration_ms: u64,
    advert_tx: Sender<GossipAdvert>,
    canister_http_adapter_client: CanisterHttpAdapterClient,
    time_source: Arc<SysTimeSource>,
) -> (
    P2PClients,
    Vec<Box<dyn JoinGuard>>,
    Arc<RwLock<IngressPoolImpl>>,
) {
    let artifact_pools = init_artifact_pools(
        node_id,
        artifact_pool_config,
        metrics_registry.clone(),
        log.clone(),
        catch_up_package,
    );

    let mut join_handles = vec![];

    let consensus_pool_cache = consensus_pool.read().unwrap().get_cache();
    let consensus_block_cache = consensus_pool.read().unwrap().get_block_cache();
    let replica_config = ReplicaConfig { node_id, subnet_id };
    let membership = Arc::new(Membership::new(
        consensus_pool_cache.clone(),
        Arc::clone(&registry_client),
        subnet_id,
    ));

    let ingress_manager = Arc::new(IngressManager::new(
        consensus_pool_cache.clone(),
        ingress_history_reader,
        artifact_pools.ingress_pool.clone(),
        Arc::clone(&registry_client),
        Arc::clone(&ingress_sig_crypto) as Arc<_>,
        metrics_registry.clone(),
        subnet_id,
        log.clone(),
        Arc::clone(&state_reader) as Arc<_>,
        cycles_account_manager,
        malicious_flags.clone(),
    ));

    let canister_http_payload_builder = Arc::new(CanisterHttpPayloadBuilderImpl::new(
        artifact_pools.canister_http_pool.clone(),
        consensus_pool_cache.clone(),
        consensus_crypto.clone(),
        state_reader.clone(),
        membership.clone(),
        subnet_id,
        registry_client.clone(),
        metrics_registry,
        log.clone(),
    ));

    let dkg_key_manager = Arc::new(Mutex::new(DkgKeyManager::new(
        metrics_registry.clone(),
        Arc::clone(&consensus_crypto),
        log.clone(),
        &PoolReader::new(&*consensus_pool.read().unwrap()),
    )));

    let consensus_client = {
        let advert_tx = advert_tx.clone();
        // Create the consensus client.
        let (client, jh) = create_consensus_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            consensus_setup(
                replica_config.clone(),
                Arc::clone(&registry_client),
                Arc::clone(&membership) as Arc<_>,
                Arc::clone(&consensus_crypto),
                Arc::clone(&ingress_manager) as Arc<_>,
                xnet_payload_builder,
                self_validating_payload_builder,
                canister_http_payload_builder,
                Arc::from(query_stats_payload_builder),
                Arc::clone(&artifact_pools.dkg_pool) as Arc<_>,
                Arc::clone(&artifact_pools.ecdsa_pool) as Arc<_>,
                Arc::clone(&dkg_key_manager) as Arc<_>,
                message_router,
                Arc::clone(&state_manager) as Arc<_>,
                Arc::clone(&time_source) as Arc<_>,
                malicious_flags.clone(),
                metrics_registry.clone(),
                log.clone(),
                local_store_time_reader,
                registry_poll_delay_duration_ms,
            ),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&consensus_pool),
            metrics_registry.clone(),
        );
        join_handles.push(jh);
        client
    };

    let ingress_client = {
        let advert_tx = advert_tx.clone();
        // Create the ingress client.
        let ingress_prioritizer = IngressPrioritizer::new(time_source.clone());
        let (client, jh) = create_ingress_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&artifact_pools.ingress_pool),
            ingress_prioritizer,
            ingress_manager,
            metrics_registry.clone(),
            malicious_flags.clone(),
        );
        join_handles.push(jh);
        client
    };

    let certification_client = {
        let advert_tx = advert_tx.clone();
        // Create the certification client.
        let (client, jh) = create_certification_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            certification_setup(
                replica_config,
                Arc::clone(&membership) as Arc<_>,
                Arc::clone(&certifier_crypto),
                Arc::clone(&state_manager) as Arc<_>,
                Arc::clone(&consensus_pool_cache) as Arc<_>,
                metrics_registry.clone(),
                log.clone(),
            ),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&artifact_pools.certification_pool),
            metrics_registry.clone(),
        );
        join_handles.push(jh);
        client
    };

    let dkg_client = {
        let advert_tx = advert_tx.clone();
        // Create the DKG client.
        let (client, jh) = create_dkg_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            (
                dkg::DkgImpl::new(
                    node_id,
                    Arc::clone(&consensus_crypto),
                    Arc::clone(&consensus_pool_cache),
                    dkg_key_manager,
                    metrics_registry.clone(),
                    log.clone(),
                ),
                dkg::DkgGossipImpl {},
            ),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&artifact_pools.dkg_pool),
            metrics_registry.clone(),
        );
        join_handles.push(jh);
        client
    };

    let ecdsa_client = {
        let finalized = consensus_pool_cache.finalized_block();
        let ecdsa_config =
            registry_client.get_ecdsa_config(subnet_id, registry_client.get_latest_version());
        info!(
            log,
            "ECDSA: finalized_height = {:?}, ecdsa_config = {:?}, \
                 DKG interval start = {:?}, is_summary = {}, has_ecdsa = {}",
            finalized.height(),
            ecdsa_config,
            finalized.payload.as_ref().dkg_interval_start_height(),
            finalized.payload.as_ref().is_summary(),
            finalized.payload.as_ref().as_ecdsa().is_some(),
        );
        let advert_tx = advert_tx.clone();
        let (client, jh) = create_ecdsa_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            (
                ecdsa::EcdsaImpl::new(
                    node_id,
                    subnet_id,
                    Arc::clone(&consensus_block_cache),
                    Arc::clone(&consensus_crypto),
                    metrics_registry.clone(),
                    log.clone(),
                    malicious_flags,
                ),
                ecdsa::EcdsaGossipImpl::new(
                    subnet_id,
                    Arc::clone(&consensus_block_cache),
                    metrics_registry.clone(),
                ),
            ),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&artifact_pools.ecdsa_pool),
            metrics_registry.clone(),
        );
        join_handles.push(jh);
        client
    };

    let https_outcalls_client = {
        let advert_tx = advert_tx.clone();
        let (client, jh) = create_https_outcalls_handlers(
            move |req| {
                let _ = advert_tx.send(req.into());
            },
            (
                CanisterHttpPoolManagerImpl::new(
                    Arc::clone(&state_reader),
                    Arc::new(Mutex::new(canister_http_adapter_client)),
                    Arc::clone(&consensus_crypto),
                    Arc::clone(&membership),
                    Arc::clone(&consensus_pool_cache),
                    ReplicaConfig { subnet_id, node_id },
                    Arc::clone(&registry_client),
                    metrics_registry.clone(),
                    log.clone(),
                ),
                CanisterHttpGossipImpl::new(
                    Arc::clone(&consensus_pool_cache),
                    Arc::clone(&state_reader),
                    log.clone(),
                ),
            ),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&artifact_pools.canister_http_pool),
            metrics_registry.clone(),
        );
        join_handles.push(jh);
        client
    };
    let p2p_clients = P2PClients {
        consensus: consensus_client,
        certification: certification_client,
        dkg: dkg_client,
        ingress: ingress_client,
        ecdsa: ecdsa_client,
        https_outcalls: https_outcalls_client,
    };
    (p2p_clients, join_handles, artifact_pools.ingress_pool)
}

fn init_artifact_pools(
    node_id: NodeId,
    config: ArtifactPoolConfig,
    registry: MetricsRegistry,
    log: ReplicaLogger,
    catch_up_package: CatchUpPackage,
) -> ArtifactPools {
    let ingress_pool = Arc::new(RwLock::new(IngressPoolImpl::new(
        node_id,
        config.clone(),
        registry.clone(),
        log.clone(),
    )));

    let mut ecdsa_pool = EcdsaPoolImpl::new_with_stats(
        config.clone(),
        log.clone(),
        registry.clone(),
        Box::new(ecdsa::EcdsaStatsImpl::new(registry.clone())),
    );
    ecdsa_pool.add_initial_dealings(&catch_up_package);
    let ecdsa_pool = Arc::new(RwLock::new(ecdsa_pool));

    let certification_pool = Arc::new(RwLock::new(CertificationPoolImpl::new(
        config,
        log.clone(),
        registry.clone(),
    )));
    let dkg_pool = Arc::new(RwLock::new(DkgPoolImpl::new(registry.clone(), log.clone())));
    let canister_http_pool = Arc::new(RwLock::new(CanisterHttpPoolImpl::new(registry, log)));
    ArtifactPools {
        ingress_pool,
        certification_pool,
        dkg_pool,
        ecdsa_pool,
        canister_http_pool,
    }
}

// The following types are used for testing only. Ideally, they should only
// appear in the test module, but `TestArtifact` is used by
// `P2PStateSyncClient` so these definitions are still required here.

#[derive(Eq, PartialEq)]
/// The artifact struct used by the testing framework.
pub struct TestArtifact;
/// The artifact message used by the testing framework.
pub type TestArtifactMessage = FileTreeSyncArtifact;
/// The artifact ID used by the testing framework.
pub type TestArtifactId = FileTreeSyncId;
/// The attribute of the artifact used by the testing framework.
pub type TestArtifactAttribute = FileTreeSyncAttribute;

/// `TestArtifact` implements the `ArtifactKind` trait.
impl ArtifactKind for TestArtifact {
    const TAG: ArtifactTag = ArtifactTag::FileTreeSyncArtifact;
    type Message = TestArtifactMessage;
    type Id = TestArtifactId;
    type Attribute = TestArtifactAttribute;
    type Filter = ();

    /// The function converts a TestArtifactMessage to an advert for a
    /// TestArtifact.
    fn message_to_advert(msg: &TestArtifactMessage) -> Advert<TestArtifact> {
        Advert {
            attribute: msg.id.to_string(),
            size: 0,
            id: msg.id.clone(),
            integrity_hash: CryptoHash(msg.id.clone().into_bytes()),
        }
    }
}
