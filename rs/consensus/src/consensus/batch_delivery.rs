//! This module contains the batch delivery logic: crafting of batches from
//! selections of ingress and xnet messages, and DKGs computed for other
//! subnets.

use crate::{
    consensus::{
        metrics::{BatchStats, BlockStats},
        status::{self, Status},
    },
    ecdsa::utils::EcdsaBlockReaderImpl,
};
use ic_artifact_pool::consensus_pool::build_consensus_block_chain;
use ic_consensus_utils::{crypto_hashable_to_seed, get_block_hash_string, pool_reader::PoolReader};
use ic_crypto::get_tecdsa_master_public_key;
use ic_https_outcalls_consensus::payload_builder::CanisterHttpPayloadBuilderImpl;
use ic_ic00_types::{EcdsaKeyId, SetupInitialDKGResponse};
use ic_interfaces::{
    batch_payload::IntoMessages,
    messaging::{MessageRouting, MessageRoutingError},
};
use ic_interfaces_registry::RegistryClient;
use ic_logger::{debug, error, info, trace, warn, ReplicaLogger};
use ic_protobuf::{
    log::consensus_log_entry::v1::ConsensusLogEntry,
    registry::{crypto::v1::PublicKey as PublicKeyProto, subnet::v1::InitialNiDkgTranscriptRecord},
};
use ic_types::{
    batch::{Batch, BatchMessages},
    consensus::{
        ecdsa::{self, CompletedSignature, EcdsaBlockReader},
        Block,
    },
    crypto::{
        canister_threshold_sig::MasterEcdsaPublicKey,
        threshold_sig::ni_dkg::{NiDkgId, NiDkgTag, NiDkgTranscript},
    },
    messages::{CallbackId, Payload, RejectContext, Response},
    CanisterId, Cycles, Height, PrincipalId, Randomness, ReplicaVersion, SubnetId,
};
use std::collections::BTreeMap;

/// Deliver all finalized blocks from
/// `message_routing.expected_batch_height` to `finalized_height` via
/// `MessageRouting` and return the last delivered batch height.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn deliver_batches(
    message_routing: &dyn MessageRouting,
    pool: &PoolReader<'_>,
    registry_client: &dyn RegistryClient,
    subnet_id: SubnetId,
    current_replica_version: ReplicaVersion,
    log: &ReplicaLogger,
    // This argument should only be used by the ic-replay tool. If it is set to `None`, we will
    // deliver all batches until the finalized height. If it is set to `Some(h)`, we will
    // deliver all bathes up to the height `min(h, finalized_height)`.
    max_batch_height_to_deliver: Option<Height>,
    result_processor: Option<&dyn Fn(&Result<(), MessageRoutingError>, BlockStats, BatchStats)>,
) -> Result<Height, MessageRoutingError> {
    let finalized_height = pool.get_finalized_height();
    // If `max_batch_height_to_deliver` is specified and smaller than
    // `finalized_height`, we use it, otherwise we use `finalized_height`.
    let target_height = max_batch_height_to_deliver
        .unwrap_or(finalized_height)
        .min(finalized_height);

    let mut h = message_routing.expected_batch_height();
    if h == Height::from(0) {
        return Ok(Height::from(0));
    }
    let mut last_delivered_batch_height = h.decrement();
    while h <= target_height {
        match (pool.get_finalized_block(h), pool.get_random_tape(h)) {
            (Some(block), Some(tape)) => {
                debug!(
                    every_n_seconds => 5,
                    log,
                    "Finalized height";
                    consensus => ConsensusLogEntry {
                        height: Some(h.get()),
                        hash: Some(get_block_hash_string(&block)),
                        replica_version: Some(String::from(current_replica_version.clone()))
                    }
                );

                if block.payload.is_summary() {
                    info!(log, "Delivering finalized batch at CUP height of {}", h);
                }
                // When we are not delivering CUP block, we must check if the subnet is halted.
                else {
                    match status::get_status(h, registry_client, subnet_id, pool, log) {
                        Some(Status::Halting | Status::Halted) => {
                            debug!(
                                every_n_seconds => 5,
                                log,
                                "Batch of height {} is not delivered because replica is halted",
                                h,
                            );
                            return Ok(last_delivered_batch_height);
                        }
                        Some(Status::Running) => {}
                        None => {
                            warn!(
                                log,
                                "Skipping batch delivery because checking if replica is halted failed",
                            );
                            return Ok(last_delivered_batch_height);
                        }
                    }
                }

                let randomness = Randomness::from(crypto_hashable_to_seed(&tape));

                let ecdsa_subnet_public_key = match get_ecdsa_subnet_public_key(&block, pool, log) {
                    Ok(maybe_key) => maybe_key,
                    Err(e) => {
                        // Do not deliver batch if we can't find a previous summary block,
                        // this means we should continue with the latest CUP.
                        warn!(
                            every_n_seconds => 5,
                            log,
                            "Do not deliver height {:?}: {}", h, e
                        );
                        return Ok(last_delivered_batch_height);
                    }
                };

                let block_stats = BlockStats::from(&block);
                let mut batch_stats = BatchStats::new(h);

                // Compute consensus' responses to subnet calls.
                let consensus_responses =
                    generate_responses_to_subnet_calls(&block, &mut batch_stats, log);

                // This flag can only be true, if we've called deliver_batches with a height
                // limit.  In this case we also want to have a checkpoint for that last height.
                let persist_batch = Some(h) == max_batch_height_to_deliver;
                let requires_full_state_hash = block.payload.is_summary() || persist_batch;
                let batch_messages = if block.payload.is_summary() {
                    BatchMessages::default()
                } else {
                    let batch_payload = &block.payload.as_ref().as_data().batch;
                    batch_stats.add_from_payload(batch_payload);
                    batch_payload
                        .clone()
                        .into_messages()
                        .map_err(|err| {
                            error!(log, "batch payload deserialization failed: {:?}", err);
                            err
                        })
                        .unwrap_or_default()
                };

                let batch = Batch {
                    batch_number: h,
                    requires_full_state_hash,
                    messages: batch_messages,
                    randomness,
                    ecdsa_subnet_public_keys: ecdsa_subnet_public_key.into_iter().collect(),
                    registry_version: block.context.registry_version,
                    time: block.context.time,
                    consensus_responses,
                };

                debug!(
                    log,
                    "replica {:?} delivered batch {:?} for block_hash {:?}",
                    current_replica_version,
                    batch_stats.batch_height,
                    block_stats.block_hash
                );
                let result = message_routing.deliver_batch(batch);
                if let Some(f) = result_processor {
                    f(&result, block_stats, batch_stats);
                }
                if let Err(err) = result {
                    warn!(every_n_seconds => 5, log, "Batch delivery failed: {:?}", err);
                    return Err(err);
                }
                last_delivered_batch_height = h;
                h = h.increment();
            }
            (None, _) => {
                trace!(
                        log,
                        "Do not deliver height {:?} because no finalized block was found. This should indicate we are waiting for state sync.",
                        h);
                break;
            }
            (_, None) => {
                // Do not deliver batch if we don't have random tape
                trace!(
                    log,
                    "Do not deliver height {:?} because RandomTape is not ready. Will re-try later",
                    h
                );
                break;
            }
        }
    }
    Ok(last_delivered_batch_height)
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
pub fn get_ecdsa_subnet_public_key(
    block: &Block,
    pool: &PoolReader<'_>,
    log: &ReplicaLogger,
) -> Result<Option<(EcdsaKeyId, MasterEcdsaPublicKey)>, String> {
    let maybe_ecdsa_and_transcript_ref = block.payload.as_ref().as_ecdsa().and_then(|ecdsa| {
        ecdsa
            .key_transcript
            .current
            .as_ref()
            .map(|unmasked| (ecdsa, *unmasked.as_ref()))
    });
    let ecdsa_subnet_public_key =
        if let Some((ecdsa, transcript_ref)) = maybe_ecdsa_and_transcript_ref {
            let summary = match pool.dkg_summary_block_for_finalized_height(block.height) {
                Some(b) => b,
                None => {
                    return Err(format!(
                        "Failed to find dkg summary block for height {}",
                        block.height
                    ))
                }
            };
            let chain = build_consensus_block_chain(pool.pool(), &summary, block);
            let block_reader = EcdsaBlockReaderImpl::new(chain);
            match block_reader.transcript(&transcript_ref) {
                Ok(transcript) => get_tecdsa_master_public_key(&transcript)
                    .ok()
                    .map(|public_key| (ecdsa.key_transcript.key_id.clone(), public_key)),
                Err(err) => {
                    warn!(
                        log,
                        "deliver_batches(): failed to translate transcript ref {:?}: {:?}",
                        transcript_ref,
                        err
                    );
                    None
                }
            }
        } else {
            None
        };
    Ok(ecdsa_subnet_public_key)
}

/// This function creates responses to the system calls that are redirected to
/// consensus. There are two types of calls being handled here:
/// - Initial NiDKG transcript creation, where a response may come from summary payloads.
/// - Threshold ECDSA signature creation, where a response may come from from data payloads.
/// - CanisterHttpResponse handling, where a response to a canister http request may come from data payloads.
pub fn generate_responses_to_subnet_calls(
    block: &Block,
    stats: &mut BatchStats,
    log: &ReplicaLogger,
) -> Vec<Response> {
    let mut consensus_responses = Vec::<Response>::new();
    let block_payload = &block.payload;
    if block_payload.is_summary() {
        let summary = block_payload.as_ref().as_summary();
        info!(
            log,
            "New DKG summary with config ids created: {:?}",
            summary.dkg.configs.keys().collect::<Vec<_>>()
        );
        consensus_responses.append(&mut generate_responses_to_setup_initial_dkg_calls(
            &summary.dkg.transcripts_for_new_subnets_with_callback_ids,
            log,
        ))
    } else {
        let block_payload = block_payload.as_ref().as_data();
        if let Some(payload) = &block_payload.ecdsa {
            consensus_responses.append(&mut generate_responses_to_sign_with_ecdsa_calls(payload));
            consensus_responses.append(&mut generate_responses_to_initial_dealings_calls(payload));
        }

        let (mut http_responses, http_stats) =
            CanisterHttpPayloadBuilderImpl::into_messages(&block_payload.batch.canister_http);
        consensus_responses.append(&mut http_responses);
        stats.canister_http = http_stats;
    }
    consensus_responses
}

struct TranscriptResults {
    low_threshold: Option<Result<NiDkgTranscript, String>>,
    high_threshold: Option<Result<NiDkgTranscript, String>>,
}

/// This function creates responses to the SetupInitialDKG system calls with the
/// computed DKG key material for remote subnets, without needing values from the state.
pub fn generate_responses_to_setup_initial_dkg_calls(
    transcripts_for_new_subnets: &[(NiDkgId, CallbackId, Result<NiDkgTranscript, String>)],
    log: &ReplicaLogger,
) -> Vec<Response> {
    let mut consensus_responses = Vec::<Response>::new();

    let mut transcripts: BTreeMap<CallbackId, TranscriptResults> = BTreeMap::new();

    for (id, callback_id, transcript) in transcripts_for_new_subnets.iter() {
        let add_transcript = |transcript_results: &mut TranscriptResults| {
            let value = Some(transcript.clone());
            match id.dkg_tag {
                NiDkgTag::LowThreshold => {
                    if transcript_results.low_threshold.is_some() {
                        error!(
                            log,
                            "Multiple low threshold transcripts for {}", callback_id
                        );
                    }
                    transcript_results.low_threshold = value;
                }
                NiDkgTag::HighThreshold => {
                    if transcript_results.high_threshold.is_some() {
                        error!(
                            log,
                            "Multiple high threshold transcripts for {}", callback_id
                        );
                    }
                    transcript_results.high_threshold = value;
                }
            }
        };
        match transcripts.get_mut(callback_id) {
            Some(existing) => add_transcript(existing),
            None => {
                let mut transcript_results = TranscriptResults {
                    low_threshold: None,
                    high_threshold: None,
                };
                add_transcript(&mut transcript_results);
                transcripts.insert(*callback_id, transcript_results);
            }
        };
    }

    for (callback_id, transcript_results) in transcripts.into_iter() {
        let payload = generate_dkg_response_payload(
            transcript_results.low_threshold.as_ref(),
            transcript_results.high_threshold.as_ref(),
            log,
        );
        if let Some(response_payload) = payload {
            consensus_responses.push(Response {
                originator: CanisterId::ic_00(),
                respondent: CanisterId::ic_00(),
                originator_reply_callback: callback_id,
                refund: Cycles::zero(),
                response_payload,
            });
        }
    }
    consensus_responses
}

/// Generate a response payload given the low and high threshold transcripts
fn generate_dkg_response_payload(
    low_threshold: Option<&Result<NiDkgTranscript, String>>,
    high_threshold: Option<&Result<NiDkgTranscript, String>>,
    log: &ReplicaLogger,
) -> Option<Payload> {
    match (low_threshold, high_threshold) {
        (Some(Ok(low_threshold_transcript)), Some(Ok(high_threshold_transcript))) => {
            info!(
                log,
                "Found transcripts for another subnet with ids {:?} and {:?}",
                low_threshold_transcript.dkg_id,
                high_threshold_transcript.dkg_id
            );
            let low_threshold_transcript_record =
                InitialNiDkgTranscriptRecord::from(low_threshold_transcript.clone());
            let high_threshold_transcript_record =
                InitialNiDkgTranscriptRecord::from(high_threshold_transcript.clone());

            // This is what we expect consensus to reply with.
            let threshold_sig_pk = high_threshold_transcript.public_key();
            let subnet_threshold_public_key = PublicKeyProto::from(threshold_sig_pk);
            let key_der: Vec<u8> =
                ic_crypto::threshold_sig_public_key_to_der(threshold_sig_pk).unwrap();
            let fresh_subnet_id =
                SubnetId::new(PrincipalId::new_self_authenticating(key_der.as_slice()));

            let initial_transcript_records = SetupInitialDKGResponse {
                low_threshold_transcript_record,
                high_threshold_transcript_record,
                fresh_subnet_id,
                subnet_threshold_public_key,
            };

            Some(Payload::Data(initial_transcript_records.encode()))
        }
        (Some(Err(err_str1)), Some(Err(err_str2))) => Some(Payload::Reject(RejectContext::new(
            ic_error_types::RejectCode::CanisterReject,
            format!("{}{}", err_str1, err_str2),
        ))),
        (Some(Err(err_str)), _) => Some(Payload::Reject(RejectContext::new(
            ic_error_types::RejectCode::CanisterReject,
            err_str,
        ))),
        (_, Some(Err(err_str))) => Some(Payload::Reject(RejectContext::new(
            ic_error_types::RejectCode::CanisterReject,
            err_str,
        ))),
        _ => None,
    }
}

/// Creates responses to `SignWithECDSA` system calls with the computed
/// signature.
pub fn generate_responses_to_sign_with_ecdsa_calls(
    ecdsa_payload: &ecdsa::EcdsaPayload,
) -> Vec<Response> {
    let mut consensus_responses = Vec::<Response>::new();
    for completed in ecdsa_payload.signature_agreements.values() {
        if let CompletedSignature::Unreported(response) = completed {
            consensus_responses.push(response.clone());
        }
    }
    consensus_responses
}

/// Creates responses to `ComputeInitialEcdsaDealingsArgs` system calls with the initial
/// dealings.
fn generate_responses_to_initial_dealings_calls(
    ecdsa_payload: &ecdsa::EcdsaPayload,
) -> Vec<Response> {
    let mut consensus_responses = Vec::<Response>::new();
    for agreement in ecdsa_payload.xnet_reshare_agreements.values() {
        if let ecdsa::CompletedReshareRequest::Unreported(response) = agreement {
            consensus_responses.push(response.clone());
        }
    }
    consensus_responses
}
