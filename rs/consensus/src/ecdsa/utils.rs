//! Common utils for the ECDSA implementation.

use crate::consensus::metrics::EcdsaPayloadMetrics;
use crate::ecdsa::complaints::{EcdsaTranscriptLoader, TranscriptLoadStatus};
use ic_artifact_pool::consensus_pool::build_consensus_block_chain;
use ic_consensus_utils::pool_reader::PoolReader;
use ic_crypto::get_tecdsa_master_public_key;
use ic_ic00_types::EcdsaKeyId;
use ic_interfaces::consensus_pool::ConsensusBlockChain;
use ic_interfaces::ecdsa::{EcdsaChangeAction, EcdsaChangeSet, EcdsaPool};
use ic_interfaces_registry::RegistryClient;
use ic_logger::{warn, ReplicaLogger};
use ic_protobuf::registry::subnet::v1 as pb;
use ic_registry_client_helpers::ecdsa_keys::EcdsaKeysRegistry;
use ic_registry_client_helpers::subnet::SubnetRegistry;
use ic_registry_subnet_features::EcdsaConfig;
use ic_types::consensus::ecdsa::QuadrupleId;
use ic_types::consensus::Block;
use ic_types::consensus::{
    ecdsa::{
        EcdsaBlockReader, EcdsaMessage, IDkgTranscriptParamsRef, RequestId,
        ThresholdEcdsaSigInputsRef, TranscriptLookupError, TranscriptRef,
    },
    HasHeight,
};
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgTranscript, IDkgTranscriptOperation, InitialIDkgDealings,
};
use ic_types::crypto::canister_threshold_sig::MasterEcdsaPublicKey;
use ic_types::registry::RegistryClientError;
use ic_types::{Height, RegistryVersion, SubnetId};
use std::collections::BTreeSet;
use std::convert::TryInto;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct InvalidChainCacheError(String);

pub(super) struct EcdsaBlockReaderImpl {
    chain: Arc<dyn ConsensusBlockChain>,
}

impl EcdsaBlockReaderImpl {
    pub(crate) fn new(chain: Arc<dyn ConsensusBlockChain>) -> Self {
        Self { chain }
    }
}

impl EcdsaBlockReader for EcdsaBlockReaderImpl {
    fn tip_height(&self) -> Height {
        self.chain.tip().height()
    }

    fn requested_transcripts(&self) -> Box<dyn Iterator<Item = &IDkgTranscriptParamsRef> + '_> {
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(Box::new(std::iter::empty()), |ecdsa_payload| {
                ecdsa_payload.iter_transcript_configs_in_creation()
            })
    }

    fn quadruples_in_creation(&self) -> Box<dyn Iterator<Item = &QuadrupleId> + '_> {
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(Box::new(std::iter::empty()), |ecdsa_payload| {
                Box::new(ecdsa_payload.quadruples_in_creation.keys())
            })
    }

    fn requested_signatures(
        &self,
    ) -> Box<dyn Iterator<Item = (&RequestId, &ThresholdEcdsaSigInputsRef)> + '_> {
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(Box::new(std::iter::empty()), |payload| {
                Box::new(payload.ongoing_signatures.iter())
            })
    }

    fn active_transcripts(&self) -> BTreeSet<TranscriptRef> {
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(BTreeSet::new(), |payload| payload.active_transcripts())
    }

    fn source_subnet_xnet_transcripts(
        &self,
    ) -> Box<dyn Iterator<Item = &IDkgTranscriptParamsRef> + '_> {
        // TODO: chain iters for multiple key_id support
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(Box::new(std::iter::empty()), |ecdsa_payload| {
                ecdsa_payload.iter_xnet_transcripts_source_subnet()
            })
    }

    fn target_subnet_xnet_transcripts(
        &self,
    ) -> Box<dyn Iterator<Item = &IDkgTranscriptParamsRef> + '_> {
        // TODO: chain iters for multiple key_id support
        self.chain
            .tip()
            .payload
            .as_ref()
            .as_ecdsa()
            .map_or(Box::new(std::iter::empty()), |ecdsa_payload| {
                ecdsa_payload.iter_xnet_transcripts_target_subnet()
            })
    }

    fn transcript(
        &self,
        transcript_ref: &TranscriptRef,
    ) -> Result<IDkgTranscript, TranscriptLookupError> {
        let ecdsa_payload = match self.chain.get_block_by_height(transcript_ref.height) {
            Ok(block) => {
                if let Some(ecdsa_payload) = block.payload.as_ref().as_ecdsa() {
                    ecdsa_payload
                } else {
                    return Err(format!(
                        "transcript(): chain look up failed {:?}: EcdsaPayload not found",
                        transcript_ref
                    ));
                }
            }
            Err(err) => {
                return Err(format!(
                    "transcript(): chain look up failed {:?}: {:?}",
                    transcript_ref, err
                ))
            }
        };

        ecdsa_payload
            .idkg_transcripts
            .get(&transcript_ref.transcript_id)
            .ok_or(format!(
                "transcript(): missing idkg_transcript: {:?}",
                transcript_ref
            ))
            .map(|entry| entry.clone())
    }
}

pub(super) fn block_chain_reader(
    pool_reader: &PoolReader<'_>,
    summary_block: &Block,
    parent_block: &Block,
    ecdsa_payload_metrics: Option<&EcdsaPayloadMetrics>,
    log: &ReplicaLogger,
) -> Result<EcdsaBlockReaderImpl, InvalidChainCacheError> {
    // Resolve the transcript refs pointing into the parent chain,
    // copy the resolved transcripts into the summary block.
    block_chain_cache(pool_reader, summary_block, parent_block)
        .map(EcdsaBlockReaderImpl::new)
        .map_err(|err| {
            warn!(
                log,
                "block_chain_reader(): failed to build chain cache: {:?}", err
            );
            if let Some(metrics) = ecdsa_payload_metrics {
                metrics.payload_errors_inc("summary_invalid_chain_cache");
            };
            err
        })
}

/// Wrapper to build the chain cache and perform sanity checks on the returned chain
pub(super) fn block_chain_cache(
    pool_reader: &PoolReader<'_>,
    start: &Block,
    end: &Block,
) -> Result<Arc<dyn ConsensusBlockChain>, InvalidChainCacheError> {
    let chain = build_consensus_block_chain(pool_reader.pool(), start, end);
    let expected_len = (end.height().get() - start.height().get() + 1) as usize;
    let chain_len = chain.len();
    if chain_len == expected_len {
        Ok(chain)
    } else {
        Err(InvalidChainCacheError(format!(
            "Invalid chain cache length: expected = {:?}, actual = {:?}, \
             start = {:?}, end = {:?}, tip = {:?}, \
             notarized_height = {:?}, finalized_height = {:?}, CUP height = {:?}",
            expected_len,
            chain_len,
            start.height(),
            end.height(),
            chain.tip().height(),
            pool_reader.get_notarized_height(),
            pool_reader.get_finalized_height(),
            pool_reader.get_catch_up_height()
        )))
    }
}

/// Load the given transcripts
/// Returns None if all the transcripts could be loaded successfully.
/// Otherwise, returns the complaint change set to be added to the pool
pub(super) fn load_transcripts(
    ecdsa_pool: &dyn EcdsaPool,
    transcript_loader: &dyn EcdsaTranscriptLoader,
    transcripts: &[&IDkgTranscript],
) -> Option<EcdsaChangeSet> {
    let mut new_complaints = Vec::new();
    for transcript in transcripts {
        match transcript_loader.load_transcript(ecdsa_pool, transcript) {
            TranscriptLoadStatus::Success => (),
            TranscriptLoadStatus::Failure => return Some(Default::default()),
            TranscriptLoadStatus::Complaints(complaints) => {
                for complaint in complaints {
                    new_complaints.push(EcdsaChangeAction::AddToValidated(
                        EcdsaMessage::EcdsaComplaint(complaint),
                    ));
                }
            }
        }
    }

    if new_complaints.is_empty() {
        None
    } else {
        Some(new_complaints)
    }
}

/// Brief summary of the IDkgTranscriptOperation
pub(super) fn transcript_op_summary(op: &IDkgTranscriptOperation) -> String {
    match op {
        IDkgTranscriptOperation::Random => "Random".to_string(),
        IDkgTranscriptOperation::ReshareOfMasked(t) => {
            format!("ReshareOfMasked({:?})", t.transcript_id)
        }
        IDkgTranscriptOperation::ReshareOfUnmasked(t) => {
            format!("ReshareOfUnmasked({:?})", t.transcript_id)
        }
        IDkgTranscriptOperation::UnmaskedTimesMasked(t1, t2) => format!(
            "UnmaskedTimesMasked({:?}, {:?})",
            t1.transcript_id, t2.transcript_id
        ),
    }
}

/// Inspect ecdsa_initializations field in the CUPContent.
/// Return key_id and dealings.
pub(crate) fn inspect_ecdsa_initializations(
    ecdsa_initializations: &[pb::EcdsaInitialization],
) -> Result<Option<(EcdsaKeyId, InitialIDkgDealings)>, String> {
    if ecdsa_initializations.is_empty() {
        return Ok(None);
    }

    if ecdsa_initializations.len() > 1 {
        return Err(
            "More than one ecdsa_initialization is not supported. Choose the first one."
                .to_string(),
        );
    }

    let ecdsa_init = ecdsa_initializations
        .get(0)
        .expect("Error: Ecdsa Initialization is None");

    let ecdsa_key_id = ecdsa_init
        .key_id
        .clone()
        .expect("Error: Failed to find key_id in ecdsa_initializations")
        .try_into()
        .map_err(|err| {
            format!(
                "Error reading ECDSA key_id: {:?}. Setting ecdsa_summary to None.",
                err
            )
        })?;

    let dealings = ecdsa_init
        .dealings
        .as_ref()
        .expect("Error: Failed to find dealings in ecdsa_initializations")
        .try_into()
        .map_err(|err| {
            format!(
                "Error reading ECDSA dealings: {:?}. Setting ecdsa_summary to None.",
                err
            )
        })?;

    Ok(Some((ecdsa_key_id, dealings)))
}

/// Return [`EcdsaConfig`] if it is enabled for the given subnet.
pub(crate) fn get_ecdsa_config_if_enabled(
    subnet_id: SubnetId,
    registry_version: RegistryVersion,
    registry_client: &dyn RegistryClient,
    log: &ReplicaLogger,
) -> Result<Option<EcdsaConfig>, RegistryClientError> {
    if let Some(mut ecdsa_config) = registry_client.get_ecdsa_config(subnet_id, registry_version)? {
        if ecdsa_config.quadruples_to_create_in_advance == 0 {
            warn!(
                log,
                "Wrong ecdsa_config: quadruples_to_create_in_advance is zero"
            );
        } else if ecdsa_config.key_ids.is_empty() {
            // This means it is not enabled
        } else if ecdsa_config.key_ids.len() > 1 {
            warn!(
                log,
                "Wrong ecdsa_config: multiple key_ids is not yet supported. Pick the first one."
            );
            ecdsa_config.key_ids = vec![ecdsa_config.key_ids[0].clone()];
            return Ok(Some(ecdsa_config));
        } else {
            return Ok(Some(ecdsa_config));
        }
    }
    Ok(None)
}

/// Return ids of ECDSA keys of the given [EcdsaConfig] for which
/// signing is enabled on the given subnet.
pub(crate) fn get_enabled_signing_keys(
    subnet_id: SubnetId,
    registry_version: RegistryVersion,
    registry_client: &dyn RegistryClient,
    ecdsa_config: &EcdsaConfig,
) -> Result<BTreeSet<EcdsaKeyId>, RegistryClientError> {
    let signing_subnets = registry_client
        .get_ecdsa_signing_subnets(registry_version)?
        .unwrap_or_default();
    Ok(ecdsa_config
        .key_ids
        .iter()
        .filter(|&key_id| match signing_subnets.get(key_id) {
            Some(subnets) => subnets.contains(&subnet_id),
            None => false,
        })
        .cloned()
        .collect())
}

/// This function returns the ECDSA subnet public key to be added to the batch, if required.
/// We return `Ok(Some(key))`, if
/// - The block contains an ECDSA payload with current key transcript ref, and
/// - the corresponding transcript exists in past blocks, and
/// - we can extract the tECDSA master public key from the transcript.
/// Otherwise `Ok(None)` is returned.
/// Additionally, we return `Err(string)` if we were unable to find a dkg summary block for the height
/// of the given block (as the lower bound for past blocks to lookup the transcript in). In that case
/// a newer CUP is already present in the pool and we should continue from there.
pub(crate) fn get_ecdsa_subnet_public_key(
    block: &Block,
    pool: &PoolReader<'_>,
    log: &ReplicaLogger,
) -> Result<Option<(EcdsaKeyId, MasterEcdsaPublicKey)>, String> {
    let Some(ecdsa_payload) = block.payload.as_ref().as_ecdsa() else {
        return Ok(None);
    };

    let Some(transcript_ref) = ecdsa_payload
        .key_transcript
        .current
        .as_ref()
        .map(|unmasked| *unmasked.as_ref())
    else {
        return Ok(None);
    };

    let Some(summary) = pool.dkg_summary_block_for_finalized_height(block.height) else {
        return Err(format!(
            "Failed to find dkg summary block for height {}",
            block.height
        ));
    };
    let chain = build_consensus_block_chain(pool.pool(), &summary, block);
    let block_reader = EcdsaBlockReaderImpl::new(chain);

    let ecdsa_subnet_public_key = match block_reader.transcript(&transcript_ref) {
        Ok(transcript) => get_ecdsa_subnet_public_key_(&transcript, log),
        Err(err) => {
            warn!(
                log,
                "Failed to translate transcript ref {:?}: {:?}", transcript_ref, err
            );

            None
        }
    };

    Ok(ecdsa_subnet_public_key
        .map(|public_key| (ecdsa_payload.key_transcript.key_id.clone(), public_key)))
}

fn get_ecdsa_subnet_public_key_(
    transcript: &IDkgTranscript,
    log: &ReplicaLogger,
) -> Option<MasterEcdsaPublicKey> {
    match get_tecdsa_master_public_key(transcript) {
        Ok(public_key) => Some(public_key),
        Err(err) => {
            warn!(log, "Failed to retrieve ECDSA subnet public key: {:?}", err);

            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use ic_config::artifact_pool::ArtifactPoolConfig;
    use ic_consensus_mocks::{dependencies, Dependencies};
    use ic_crypto_test_utils_canister_threshold_sigs::dummy_values::dummy_initial_idkg_dealing_for_tests;
    use ic_crypto_test_utils_reproducible_rng::reproducible_rng;
    use ic_logger::replica_logger::no_op_logger;
    use ic_protobuf::registry::{
        crypto::v1::EcdsaSigningSubnetList, subnet::v1::EcdsaInitialization,
    };
    use ic_registry_client_fake::FakeRegistryClient;
    use ic_registry_keys::make_ecdsa_signing_subnet_list_key;
    use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
    use ic_test_utilities::types::ids::{node_test_id, subnet_test_id};
    use ic_test_utilities_registry::{add_subnet_record, SubnetRecordBuilder};
    use ic_types::subnet_id_into_protobuf;

    use super::*;

    #[test]
    fn test_inspect_ecdsa_initializations_no_keys() {
        let init =
            inspect_ecdsa_initializations(&[]).expect("Should successfully get initializations");

        assert!(init.is_none());
    }

    #[test]
    fn test_inspect_ecdsa_initializations_one_key() {
        let mut rng = reproducible_rng();
        let initial_dealings = dummy_initial_idkg_dealing_for_tests(
            ic_types::crypto::AlgorithmId::ThresholdEcdsaSecp256k1,
            &mut rng,
        );
        let key_id = EcdsaKeyId::from_str("Secp256k1:some_key").unwrap();
        let ecdsa_init = EcdsaInitialization {
            key_id: Some((&key_id).into()),
            dealings: Some((&initial_dealings).into()),
        };

        let init = inspect_ecdsa_initializations(&[ecdsa_init])
            .expect("Should successfully get initializations");

        assert_eq!(init, Some((key_id, initial_dealings)));
    }

    #[test]
    fn test_inspect_ecdsa_initializations_multiple_keys() {
        let mut rng = reproducible_rng();
        let initial_dealings = dummy_initial_idkg_dealing_for_tests(
            ic_types::crypto::AlgorithmId::ThresholdEcdsaSecp256k1,
            &mut rng,
        );
        let key_id = EcdsaKeyId::from_str("Secp256k1:some_key").unwrap();
        let key_id_2 = EcdsaKeyId::from_str("Secp256k1:some_key_2").unwrap();
        let ecdsa_init = EcdsaInitialization {
            key_id: Some((&key_id).into()),
            dealings: Some((&initial_dealings).into()),
        };
        let ecdsa_init_2 = EcdsaInitialization {
            key_id: Some((&key_id_2).into()),
            dealings: Some((&initial_dealings).into()),
        };

        inspect_ecdsa_initializations(&[ecdsa_init.clone(), ecdsa_init_2.clone()])
            .expect_err("Should fail because of the multiple keys");
    }

    fn set_up_get_ecdsa_config_test(
        config: &EcdsaConfig,
        pool_config: ArtifactPoolConfig,
    ) -> (SubnetId, Arc<FakeRegistryClient>, RegistryVersion) {
        let Dependencies {
            registry,
            registry_data_provider,
            ..
        } = dependencies(pool_config, 1);
        let subnet_id = subnet_test_id(1);
        let registry_version = RegistryVersion::from(10);

        add_subnet_record(
            &registry_data_provider,
            registry_version.get(),
            subnet_id,
            SubnetRecordBuilder::from(&[node_test_id(0)])
                .with_ecdsa_config(config.clone())
                .build(),
        );
        registry.update_to_latest_version();

        (subnet_id, registry, registry_version)
    }

    #[test]
    fn test_get_ecdsa_config_if_enabled_no_keys() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let ecdsa_config_with_no_keys = EcdsaConfig {
                quadruples_to_create_in_advance: 1,
                key_ids: vec![],
                max_queue_size: Some(3),
                ..EcdsaConfig::default()
            };
            let (subnet_id, registry, version) =
                set_up_get_ecdsa_config_test(&ecdsa_config_with_no_keys, pool_config);

            let config =
                get_ecdsa_config_if_enabled(subnet_id, version, registry.as_ref(), &no_op_logger())
                    .expect("Should successfully get the config");

            assert!(config.is_none());
        })
    }

    #[test]
    fn test_get_ecdsa_config_if_enabled_one_key() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let ecdsa_config_with_one_key = EcdsaConfig {
                quadruples_to_create_in_advance: 1,
                key_ids: vec![EcdsaKeyId::from_str("Secp256k1:some_key").unwrap()],
                max_queue_size: Some(3),
                ..EcdsaConfig::default()
            };
            let (subnet_id, registry, version) =
                set_up_get_ecdsa_config_test(&ecdsa_config_with_one_key, pool_config);

            let config =
                get_ecdsa_config_if_enabled(subnet_id, version, registry.as_ref(), &no_op_logger())
                    .expect("Should successfully get the config");

            assert_eq!(config, Some(ecdsa_config_with_one_key));
        })
    }

    #[test]
    fn test_get_ecdsa_config_if_enabled_multiple_keys() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let key_id = EcdsaKeyId::from_str("Secp256k1:some_key_1").unwrap();
            let key_id_2 = EcdsaKeyId::from_str("Secp256k1:some_key_2").unwrap();
            let ecdsa_config_with_two_keys = EcdsaConfig {
                quadruples_to_create_in_advance: 1,
                key_ids: vec![key_id.clone(), key_id_2.clone()],
                max_queue_size: Some(3),
                ..EcdsaConfig::default()
            };
            let (subnet_id, registry, version) =
                set_up_get_ecdsa_config_test(&ecdsa_config_with_two_keys, pool_config);

            let config =
                get_ecdsa_config_if_enabled(subnet_id, version, registry.as_ref(), &no_op_logger())
                    .expect("Should successfully get the config");

            assert_eq!(
                config,
                Some(EcdsaConfig {
                    key_ids: vec![key_id],
                    ..ecdsa_config_with_two_keys
                })
            );
        })
    }

    #[test]
    fn test_get_ecdsa_config_if_enabled_malformed() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let malformed_ecdsa_config = EcdsaConfig {
                quadruples_to_create_in_advance: 0,
                key_ids: vec![EcdsaKeyId::from_str("Secp256k1:some_key").unwrap()],
                max_queue_size: Some(3),
                ..EcdsaConfig::default()
            };
            let (subnet_id, registry, version) =
                set_up_get_ecdsa_config_test(&malformed_ecdsa_config, pool_config);

            let config =
                get_ecdsa_config_if_enabled(subnet_id, version, registry.as_ref(), &no_op_logger())
                    .expect("Should successfully get the config");

            assert!(config.is_none());
        })
    }

    #[test]
    fn test_get_enabled_signing_keys() {
        let key_id1 = EcdsaKeyId::from_str("Secp256k1:some_key1").unwrap();
        let key_id2 = EcdsaKeyId::from_str("Secp256k1:some_key2").unwrap();
        let key_id3 = EcdsaKeyId::from_str("Secp256k1:some_key3").unwrap();
        let ecdsa_config = EcdsaConfig {
            key_ids: vec![key_id1.clone(), key_id2.clone()],
            ..EcdsaConfig::default()
        };
        let registry_data = Arc::new(ProtoRegistryDataProvider::new());
        let registry = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
        let subnet_id = subnet_test_id(1);

        let add_key = |version, key_id, subnets| {
            registry_data
                .add(
                    &make_ecdsa_signing_subnet_list_key(key_id),
                    RegistryVersion::from(version),
                    Some(EcdsaSigningSubnetList { subnets }),
                )
                .expect("failed to add subnets to registry");
        };

        add_key(1, &key_id1, vec![subnet_id_into_protobuf(subnet_id)]);
        add_key(2, &key_id2, vec![subnet_id_into_protobuf(subnet_id)]);
        add_key(2, &key_id3, vec![subnet_id_into_protobuf(subnet_id)]);
        add_key(3, &key_id1, vec![]);
        registry.update_to_latest_version();

        let test_cases = vec![
            (0, Ok(BTreeSet::new())),
            (1, Ok(BTreeSet::from_iter(vec![key_id1.clone()]))),
            (2, Ok(BTreeSet::from_iter(vec![key_id1, key_id2.clone()]))),
            (3, Ok(BTreeSet::from_iter(vec![key_id2]))),
            (
                4,
                Err(RegistryClientError::VersionNotAvailable {
                    version: RegistryVersion::from(4),
                }),
            ),
        ];

        for (version, expected) in test_cases {
            let result = get_enabled_signing_keys(
                subnet_id,
                RegistryVersion::from(version),
                registry.as_ref(),
                &ecdsa_config,
            );
            assert_eq!(result, expected);
        }
    }
}
