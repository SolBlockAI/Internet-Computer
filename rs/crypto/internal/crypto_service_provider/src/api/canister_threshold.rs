//! CSP canister threshold signature traits

use ic_crypto_internal_threshold_sig_ecdsa::{
    CommitmentOpening, IDkgComplaintInternal, IDkgDealingInternal, IDkgTranscriptInternal,
    IDkgTranscriptOperationInternal, MEGaPublicKey, ThresholdEcdsaCombinedSigInternal,
    ThresholdEcdsaSigShareInternal,
};
use ic_protobuf::registry::crypto::v1::PublicKey;
use ic_types::crypto::canister_threshold_sig::error::{
    IDkgCreateTranscriptError, IDkgLoadTranscriptError, IDkgOpenTranscriptError,
    IDkgRetainKeysError, IDkgVerifyComplaintError, IDkgVerifyDealingPrivateError,
    IDkgVerifyDealingPublicError, IDkgVerifyOpeningError, IDkgVerifyTranscriptError,
    ThresholdEcdsaCombineSigSharesError, ThresholdEcdsaSignShareError,
    ThresholdEcdsaVerifyCombinedSignatureError, ThresholdEcdsaVerifySigShareError,
};
use ic_types::crypto::canister_threshold_sig::{
    idkg::{BatchSignedIDkgDealing, IDkgTranscriptOperation},
    ExtendedDerivationPath, ThresholdEcdsaSigInputs,
};
use ic_types::crypto::AlgorithmId;
use ic_types::{NodeIndex, NumberOfNodes, Randomness, RegistryVersion};
use std::collections::{BTreeMap, BTreeSet};

pub mod errors;
pub use errors::*;

use crate::vault::api::{
    IDkgCreateDealingVaultError, IDkgDealingInternalBytes, IDkgTranscriptInternalBytes,
};

/// Crypto service provider (CSP) client for interactive distributed key
/// generation (IDkg) for canister threshold signatures.
pub trait CspIDkgProtocol {
    /// Generates a share of a dealing for a single receiver.
    fn idkg_create_dealing(
        &self,
        algorithm_id: AlgorithmId,
        context_data: Vec<u8>,
        dealer_index: NodeIndex,
        reconstruction_threshold: NumberOfNodes,
        receiver_keys: Vec<PublicKey>,
        transcript_operation: IDkgTranscriptOperation,
    ) -> Result<IDkgDealingInternalBytes, IDkgCreateDealingVaultError>;

    /// Performs private verification of a dealing.
    fn idkg_verify_dealing_private(
        &self,
        algorithm_id: AlgorithmId,
        dealing: IDkgDealingInternalBytes,
        dealer_index: NodeIndex,
        receiver_index: NodeIndex,
        receiver_public_key: MEGaPublicKey,
        context_data: Vec<u8>,
    ) -> Result<(), IDkgVerifyDealingPrivateError>;

    /// Verify the public parts of a dealing
    fn idkg_verify_dealing_public(
        &self,
        algorithm_id: AlgorithmId,
        dealing: &IDkgDealingInternal,
        operation_mode: &IDkgTranscriptOperationInternal,
        reconstruction_threshold: NumberOfNodes,
        dealer_index: NodeIndex,
        number_of_receivers: NumberOfNodes,
        context_data: &[u8],
    ) -> Result<(), IDkgVerifyDealingPublicError>;

    /// Generates an IDkg transcript from verified IDkg dealings
    fn idkg_create_transcript(
        &self,
        algorithm_id: AlgorithmId,
        reconstruction_threshold: NumberOfNodes,
        verified_dealings: &BTreeMap<NodeIndex, IDkgDealingInternal>,
        operation_mode: &IDkgTranscriptOperationInternal,
    ) -> Result<IDkgTranscriptInternal, IDkgCreateTranscriptError>;

    fn idkg_verify_transcript(
        &self,
        transcript: &IDkgTranscriptInternal,
        algorithm_id: AlgorithmId,
        reconstruction_threshold: NumberOfNodes,
        verified_dealings: &BTreeMap<NodeIndex, IDkgDealingInternal>,
        operation_mode: &IDkgTranscriptOperationInternal,
    ) -> Result<(), IDkgVerifyTranscriptError>;

    /// Compute secret from transcript and store in SKS, generating complaints
    /// if necessary.
    fn idkg_load_transcript(
        &self,
        dealings: BTreeMap<NodeIndex, BatchSignedIDkgDealing>,
        context_data: Vec<u8>,
        receiver_index: NodeIndex,
        public_key: MEGaPublicKey,
        transcript: IDkgTranscriptInternalBytes,
    ) -> Result<BTreeMap<NodeIndex, IDkgComplaintInternal>, IDkgLoadTranscriptError>;

    /// Computes a secret share from a transcript and openings, and stores it
    /// in the canister secret key store.
    fn idkg_load_transcript_with_openings(
        &self,
        dealings: BTreeMap<NodeIndex, BatchSignedIDkgDealing>,
        openings: BTreeMap<NodeIndex, BTreeMap<NodeIndex, CommitmentOpening>>,
        context_data: Vec<u8>,
        receiver_index: NodeIndex,
        public_key: MEGaPublicKey,
        transcript: IDkgTranscriptInternalBytes,
    ) -> Result<(), IDkgLoadTranscriptError>;

    /// Generate a MEGa public/private key pair for encrypting threshold key shares in transmission
    /// from dealers to receivers. The generated public key will be stored in the node's public key store
    /// while the private key will be stored in the node's secret key store.
    ///
    /// # Returns
    /// Generated public key.
    ///
    /// # Errors
    /// * [`CspCreateMEGaKeyError::SerializationError`] if serialization of public or private key
    ///   before storing it in their respective key store failed.
    /// * [`CspCreateMEGaKeyError::TransientInternalError`] if there is a
    ///   transient internal error, e.g,. an IO error when writing a key to
    ///   disk, or an RPC error when calling a remote CSP vault.
    /// * [`CspCreateMEGaKeyError::DuplicateKeyId`] if there already
    ///   exists a secret key in the store for the secret key ID derived from
    ///   the public part of the randomly generated key pair. This error
    ///   most likely indicates a bad randomness source.
    /// * [`CspCreateMEGaKeyError::InternalError`]: if the key ID for the secret key cannot be
    ///   derived from the generated public key.
    fn idkg_gen_dealing_encryption_key_pair(&self) -> Result<MEGaPublicKey, CspCreateMEGaKeyError>;

    /// Verifies that the given `complaint` about `dealing` is correct/justified.
    /// A complaint is created, e.g., when loading of a transcript fails.
    fn idkg_verify_complaint(
        &self,
        complaint: &IDkgComplaintInternal,
        complainer_index: NodeIndex,
        complainer_key: &MEGaPublicKey,
        dealing: &IDkgDealingInternal,
        dealer_index: NodeIndex,
        context_data: &[u8],
    ) -> Result<(), IDkgVerifyComplaintError>;

    /// Opens `dealing`.
    fn idkg_open_dealing(
        &self,
        dealing: BatchSignedIDkgDealing,
        dealer_index: NodeIndex,
        context_data: Vec<u8>,
        opener_index: NodeIndex,
        opener_public_key: MEGaPublicKey,
    ) -> Result<CommitmentOpening, IDkgOpenTranscriptError>;

    /// Verifies an `opening` of `dealing`.
    fn idkg_verify_dealing_opening(
        &self,
        dealing: IDkgDealingInternal,
        opener_index: NodeIndex,
        opening: CommitmentOpening,
    ) -> Result<(), IDkgVerifyOpeningError>;

    /// Retains IDKG key material for the given transcripts.
    fn idkg_retain_active_keys(
        &self,
        active_transcripts: BTreeSet<IDkgTranscriptInternal>,
        oldest_public_key: MEGaPublicKey,
    ) -> Result<(), IDkgRetainKeysError>;

    /// Make a metrics observation of the minimum registry version in active iDKG transcripts.
    fn idkg_observe_minimum_registry_version_in_active_idkg_transcripts(
        &self,
        registry_version: RegistryVersion,
    );
}

/// Crypto service provider (CSP) client for threshold ECDSA signature share
/// generation.
pub trait CspThresholdEcdsaSigner {
    /// Generate a signature share.
    fn ecdsa_sign_share(
        &self,
        inputs: &ThresholdEcdsaSigInputs,
    ) -> Result<ThresholdEcdsaSigShareInternal, ThresholdEcdsaSignShareError>;
}

/// Crypto service provider (CSP) client for threshold ECDSA signature
/// verification.
pub trait CspThresholdEcdsaSigVerifier {
    /// Combine signature shares.
    #[allow(clippy::too_many_arguments)]
    fn ecdsa_combine_sig_shares(
        &self,
        derivation_path: &ExtendedDerivationPath,
        hashed_message: &[u8],
        nonce: &Randomness,
        key: &IDkgTranscriptInternal,
        kappa_unmasked: &IDkgTranscriptInternal,
        reconstruction_threshold: NumberOfNodes,
        sig_shares: &BTreeMap<NodeIndex, ThresholdEcdsaSigShareInternal>,
        algorithm_id: AlgorithmId,
    ) -> Result<ThresholdEcdsaCombinedSigInternal, ThresholdEcdsaCombineSigSharesError>;

    /// Verify a signature share
    fn ecdsa_verify_sig_share(
        &self,
        share: &ThresholdEcdsaSigShareInternal,
        signer_index: NodeIndex,
        derivation_path: &ExtendedDerivationPath,
        hashed_message: &[u8],
        nonce: &Randomness,
        key: &IDkgTranscriptInternal,
        kappa_unmasked: &IDkgTranscriptInternal,
        lambda_masked: &IDkgTranscriptInternal,
        kappa_times_lambda: &IDkgTranscriptInternal,
        key_times_lambda: &IDkgTranscriptInternal,
        algorithm_id: AlgorithmId,
    ) -> Result<(), ThresholdEcdsaVerifySigShareError>;

    /// Verify a combined ECDSA signature with respect to a particular kappa transcript
    fn ecdsa_verify_combined_signature(
        &self,
        signature: &ThresholdEcdsaCombinedSigInternal,
        derivation_path: &ExtendedDerivationPath,
        hashed_message: &[u8],
        nonce: &Randomness,
        key: &IDkgTranscriptInternal,
        kappa_unmasked: &IDkgTranscriptInternal,
        algorithm_id: AlgorithmId,
    ) -> Result<(), ThresholdEcdsaVerifyCombinedSignatureError>;
}
