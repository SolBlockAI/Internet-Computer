//! Canister Http Artifact Pool implementation.

// TODO: Remove
#![allow(dead_code)]
use crate::{
    metrics::{POOL_TYPE_UNVALIDATED, POOL_TYPE_VALIDATED},
    pool_common::PoolSection,
};
use ic_interfaces::{
    artifact_pool::{
        ChangeResult, MutablePool, UnvalidatedArtifact, ValidatedArtifact, ValidatedPoolReader,
    },
    canister_http::{CanisterHttpChangeAction, CanisterHttpChangeSet, CanisterHttpPool},
    time_source::TimeSource,
};
use ic_logger::{warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_types::{
    artifact::{ArtifactKind, CanisterHttpResponseId},
    artifact_kind::CanisterHttpArtifact,
    canister_http::{CanisterHttpResponse, CanisterHttpResponseShare},
    crypto::CryptoHashOf,
    time::current_time,
};
use prometheus::IntCounter;

const POOL_CANISTER_HTTP: &str = "canister_http";
const POOL_CANISTER_HTTP_CONTENT: &str = "canister_http_content";

type ValidatedCanisterHttpPoolSection = PoolSection<
    CryptoHashOf<CanisterHttpResponseShare>,
    ValidatedArtifact<CanisterHttpResponseShare>,
>;

type UnvalidatedCanisterHttpPoolSection = PoolSection<
    CryptoHashOf<CanisterHttpResponseShare>,
    UnvalidatedArtifact<CanisterHttpResponseShare>,
>;

type ContentCanisterHttpPoolSection =
    PoolSection<CryptoHashOf<CanisterHttpResponse>, CanisterHttpResponse>;

pub struct CanisterHttpPoolImpl {
    validated: ValidatedCanisterHttpPoolSection,
    unvalidated: UnvalidatedCanisterHttpPoolSection,
    content: ContentCanisterHttpPoolSection,
    invalidated_artifacts: IntCounter,
    log: ReplicaLogger,
}

impl CanisterHttpPoolImpl {
    pub fn new(metrics: MetricsRegistry, log: ReplicaLogger) -> Self {
        Self {
            invalidated_artifacts: metrics.int_counter(
                "canister_http_invalidated_artifacts",
                "The number of invalidated canister http artifacts",
            ),
            validated: PoolSection::new(metrics.clone(), POOL_CANISTER_HTTP, POOL_TYPE_VALIDATED),
            unvalidated: PoolSection::new(
                metrics.clone(),
                POOL_CANISTER_HTTP,
                POOL_TYPE_UNVALIDATED,
            ),
            content: ContentCanisterHttpPoolSection::new(
                metrics,
                POOL_CANISTER_HTTP_CONTENT,
                POOL_TYPE_VALIDATED,
            ),
            log,
        }
    }
}

impl CanisterHttpPool for CanisterHttpPoolImpl {
    fn get_validated_shares(&self) -> Box<dyn Iterator<Item = &CanisterHttpResponseShare> + '_> {
        Box::new(self.validated.values().map(|artifact| &artifact.msg))
    }

    fn get_unvalidated_shares(&self) -> Box<dyn Iterator<Item = &CanisterHttpResponseShare> + '_> {
        Box::new(self.unvalidated.values().map(|artifact| &artifact.message))
    }

    fn get_response_content_items(
        &self,
    ) -> Box<dyn Iterator<Item = (&CryptoHashOf<CanisterHttpResponse>, &CanisterHttpResponse)> + '_>
    {
        Box::new(self.content.iter())
    }

    fn get_response_content_by_hash(
        &self,
        hash: &CryptoHashOf<CanisterHttpResponse>,
    ) -> Option<CanisterHttpResponse> {
        self.content.get(hash).cloned()
    }

    fn lookup_validated(
        &self,
        msg_id: &CanisterHttpResponseId,
    ) -> Option<CanisterHttpResponseShare> {
        self.validated.get(msg_id).map(|s| s.msg.clone())
    }

    fn lookup_unvalidated(
        &self,
        msg_id: &CanisterHttpResponseId,
    ) -> Option<CanisterHttpResponseShare> {
        self.unvalidated.get(msg_id).map(|s| s.message.clone())
    }
}

impl MutablePool<CanisterHttpArtifact, CanisterHttpChangeSet> for CanisterHttpPoolImpl {
    fn insert(&mut self, artifact: UnvalidatedArtifact<CanisterHttpResponseShare>) {
        self.unvalidated
            .insert(ic_types::crypto::crypto_hash(&artifact.message), artifact);
    }

    fn remove(&mut self, id: &CanisterHttpResponseId) {
        self.unvalidated.remove(id);
    }

    fn apply_changes(
        &mut self,
        _time_source: &dyn TimeSource,
        change_set: CanisterHttpChangeSet,
    ) -> ChangeResult<CanisterHttpArtifact> {
        let changed = !change_set.is_empty();
        let mut adverts = Vec::new();
        let mut purged = Vec::new();
        for action in change_set {
            match action {
                CanisterHttpChangeAction::AddToValidated(share, content) => {
                    adverts.push(CanisterHttpArtifact::message_to_advert(&share));
                    self.validated.insert(
                        ic_types::crypto::crypto_hash(&share),
                        ValidatedArtifact {
                            msg: share,
                            timestamp: current_time(),
                        },
                    );
                    self.content
                        .insert(ic_types::crypto::crypto_hash(&content), content);
                }
                CanisterHttpChangeAction::MoveToValidated(share) => {
                    let id = ic_types::crypto::crypto_hash(&share);
                    match self.unvalidated.remove(&id) {
                        None => (),
                        Some(value) => {
                            adverts.push(CanisterHttpArtifact::message_to_advert(&share));
                            self.validated.insert(
                                id,
                                ValidatedArtifact {
                                    msg: value.message,
                                    timestamp: current_time(),
                                },
                            );
                        }
                    }
                }
                CanisterHttpChangeAction::RemoveValidated(id) => {
                    if self.validated.remove(&id).is_some() {
                        purged.push(id);
                    }
                }
                CanisterHttpChangeAction::RemoveUnvalidated(id) => {
                    self.remove(&id);
                }
                CanisterHttpChangeAction::RemoveContent(id) => {
                    self.content.remove(&id);
                }
                CanisterHttpChangeAction::HandleInvalid(id, reason) => {
                    self.invalidated_artifacts.inc();
                    warn!(
                        self.log,
                        "Invalid CanisterHttp message ({:?}): {:?}", reason, id
                    );
                    self.remove(&id);
                }
            }
        }
        ChangeResult {
            purged,
            adverts,
            changed,
        }
    }
}

impl ValidatedPoolReader<CanisterHttpArtifact> for CanisterHttpPoolImpl {
    fn contains(&self, id: &CanisterHttpResponseId) -> bool {
        self.unvalidated.contains_key(id) || self.validated.contains_key(id)
    }

    fn get_validated_by_identifier(
        &self,
        id: &CanisterHttpResponseId,
    ) -> Option<CanisterHttpResponseShare> {
        self.validated
            .get(id)
            .map(|artifact| (&artifact.msg))
            .cloned()
    }

    fn get_all_validated_by_filter(
        &self,
        _filter: &(),
    ) -> Box<dyn Iterator<Item = CanisterHttpResponseShare> + '_> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use ic_interfaces::time_source::SysTimeSource;
    use ic_logger::replica_logger::no_op_logger;
    use ic_test_utilities::{consensus::fake::FakeSigner, mock_time, types::ids::node_test_id};
    use ic_types::{
        canister_http::{CanisterHttpResponseContent, CanisterHttpResponseMetadata},
        crypto::{CryptoHash, Signed},
        messages::CallbackId,
        signature::BasicSignature,
        CanisterId, RegistryVersion,
    };

    use super::*;

    fn to_unvalidated(
        message: CanisterHttpResponseShare,
    ) -> UnvalidatedArtifact<CanisterHttpResponseShare> {
        UnvalidatedArtifact::<CanisterHttpResponseShare> {
            message,
            peer_id: node_test_id(0),
            timestamp: mock_time(),
        }
    }

    fn fake_share(id: u64) -> CanisterHttpResponseShare {
        Signed {
            content: CanisterHttpResponseMetadata {
                id: CallbackId::from(id),
                timeout: mock_time(),
                content_hash: CryptoHashOf::from(CryptoHash(vec![1, 2, 3])),
                registry_version: RegistryVersion::from(id),
            },
            signature: BasicSignature::fake(node_test_id(id)),
        }
    }

    fn fake_response(id: u64) -> CanisterHttpResponse {
        CanisterHttpResponse {
            id: CallbackId::from(id),
            timeout: mock_time(),
            canister_id: CanisterId::from_u64(id),
            content: CanisterHttpResponseContent::Success(Vec::new()),
        }
    }

    #[test]
    fn test_canister_http_pool_insert_and_remove() {
        let mut pool = CanisterHttpPoolImpl::new(MetricsRegistry::new(), no_op_logger());
        let share = fake_share(123);
        let id = ic_types::crypto::crypto_hash(&share);

        pool.insert(to_unvalidated(share.clone()));
        assert!(pool.contains(&id));

        assert_eq!(share, pool.lookup_unvalidated(&id).unwrap());

        pool.remove(&id);
        assert!(!pool.contains(&id));
    }

    #[test]
    fn test_canister_http_pool_add_and_remove_validated() {
        let mut pool = CanisterHttpPoolImpl::new(MetricsRegistry::new(), no_op_logger());
        let share = fake_share(123);
        let id = ic_types::crypto::crypto_hash(&share);
        let response = fake_response(123);
        let content_hash = ic_types::crypto::crypto_hash(&response);

        let result = pool.apply_changes(
            &SysTimeSource::new(),
            vec![
                CanisterHttpChangeAction::AddToValidated(share.clone(), response.clone()),
                CanisterHttpChangeAction::AddToValidated(fake_share(456), fake_response(456)),
            ],
        );

        assert!(pool.contains(&id));
        assert_eq!(result.adverts[0].id, id);
        assert!(result.changed);
        assert!(result.purged.is_empty());
        assert_eq!(share, pool.lookup_validated(&id).unwrap());
        assert_eq!(share, pool.get_validated_by_identifier(&id).unwrap());
        assert_eq!(
            response,
            pool.get_response_content_by_hash(&content_hash).unwrap()
        );

        let result = pool.apply_changes(
            &SysTimeSource::new(),
            vec![
                CanisterHttpChangeAction::RemoveValidated(id.clone()),
                CanisterHttpChangeAction::RemoveContent(content_hash.clone()),
            ],
        );

        assert!(!pool.contains(&id));
        assert!(result.adverts.is_empty());
        assert!(result.changed);
        assert_eq!(result.purged[0], id);
        assert!(pool.lookup_validated(&id).is_none());
        assert!(pool.get_response_content_by_hash(&content_hash).is_none());
        assert_eq!(pool.get_validated_shares().count(), 1);
        assert_eq!(pool.get_response_content_items().count(), 1);
    }

    #[test]
    fn test_canister_http_pool_move_to_validated() {
        let mut pool = CanisterHttpPoolImpl::new(MetricsRegistry::new(), no_op_logger());
        let share1 = fake_share(123);
        let id1 = ic_types::crypto::crypto_hash(&share1);
        let share2 = fake_share(456);
        let id2 = ic_types::crypto::crypto_hash(&share2);

        pool.insert(to_unvalidated(share1.clone()));

        let result = pool.apply_changes(
            &SysTimeSource::new(),
            vec![
                CanisterHttpChangeAction::MoveToValidated(share2.clone()),
                CanisterHttpChangeAction::MoveToValidated(share1.clone()),
            ],
        );

        assert!(pool.contains(&id1));
        assert!(!pool.contains(&id2));
        assert_eq!(result.adverts[0].id, id1);
        assert!(result.changed);
        assert!(result.purged.is_empty());
        assert_eq!(share1, pool.lookup_validated(&id1).unwrap());
    }

    #[test]
    fn test_canister_http_pool_remove_unvalidated() {
        let mut pool = CanisterHttpPoolImpl::new(MetricsRegistry::new(), no_op_logger());
        let share = fake_share(123);
        let id = ic_types::crypto::crypto_hash(&share);

        pool.insert(to_unvalidated(share.clone()));
        assert!(pool.contains(&id));

        let result = pool.apply_changes(
            &SysTimeSource::new(),
            vec![CanisterHttpChangeAction::RemoveUnvalidated(id.clone())],
        );

        assert!(!pool.contains(&id));
        assert!(result.changed);
        assert!(result.purged.is_empty());
        assert!(result.adverts.is_empty());
    }

    #[test]
    fn test_canister_http_pool_handle_invalid() {
        let mut pool = CanisterHttpPoolImpl::new(MetricsRegistry::new(), no_op_logger());
        let share = fake_share(123);
        let id = ic_types::crypto::crypto_hash(&share);

        pool.insert(to_unvalidated(share.clone()));
        assert!(pool.contains(&id));

        let result = pool.apply_changes(
            &SysTimeSource::new(),
            vec![CanisterHttpChangeAction::HandleInvalid(
                id.clone(),
                "TEST REASON".to_string(),
            )],
        );

        assert!(!pool.contains(&id));
        assert!(result.changed);
        assert!(result.purged.is_empty());
        assert!(result.adverts.is_empty());
    }
}
