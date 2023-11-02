use ic_artifact_pool::ingress_pool::IngressPoolImpl;
use ic_config::artifact_pool::ArtifactPoolConfig;
use ic_interfaces::{
    artifact_pool::{ChangeResult, MutablePool, UnvalidatedArtifact},
    ingress_pool::{
        ChangeSet, IngressPool, IngressPoolObject, IngressPoolSelect, IngressPoolThrottler,
        PoolSection, SelectResult, UnvalidatedIngressArtifact, ValidatedIngressArtifact,
    },
    time_source::TimeSource,
};
use ic_logger::replica_logger::no_op_logger;
use ic_metrics::MetricsRegistry;
use ic_types::{
    artifact::IngressMessageId, artifact_kind::IngressArtifact, messages::SignedIngress, NodeId,
    Time,
};

pub struct TestIngressPool {
    pub pool: IngressPoolImpl,
}

impl TestIngressPool {
    pub fn new(node_id: NodeId, pool_config: ArtifactPoolConfig) -> TestIngressPool {
        TestIngressPool {
            pool: IngressPoolImpl::new(
                node_id,
                pool_config,
                MetricsRegistry::new(),
                no_op_logger(),
            ),
        }
    }
}

impl IngressPool for TestIngressPool {
    fn validated(&self) -> &dyn PoolSection<ValidatedIngressArtifact> {
        self.pool.validated()
    }

    fn unvalidated(&self) -> &dyn PoolSection<UnvalidatedIngressArtifact> {
        self.pool.unvalidated()
    }
}

impl IngressPoolThrottler for TestIngressPool {
    fn exceeds_threshold(&self) -> bool {
        self.pool.exceeds_threshold()
    }
}

impl MutablePool<IngressArtifact, ChangeSet> for TestIngressPool {
    fn insert(&mut self, unvalidated_artifact: UnvalidatedArtifact<SignedIngress>) {
        self.pool.insert(unvalidated_artifact)
    }

    fn remove(&mut self, id: &IngressMessageId) {
        self.pool.remove(id)
    }

    fn apply_changes(
        &mut self,
        time_source: &dyn TimeSource,
        change_set: ChangeSet,
    ) -> ChangeResult<IngressArtifact> {
        self.pool.apply_changes(time_source, change_set)
    }
}

impl IngressPoolSelect for TestIngressPool {
    fn select_validated<'a>(
        &self,
        range: std::ops::RangeInclusive<Time>,
        f: Box<dyn FnMut(&IngressPoolObject) -> SelectResult<SignedIngress> + 'a>,
    ) -> Vec<SignedIngress> {
        self.pool.select_validated(range, f)
    }
}
