#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
//#![deny(missing_docs)]

//! Interface for the cryptographic service provider

pub mod api;
pub mod builder;
pub mod canister_threshold;
#[cfg(test)]
pub mod imported_test_utils;
pub mod imported_utilities;
pub mod key_id;
pub mod keygen;
pub mod public_key_store;
pub mod secret_key_store;
mod signer;
pub mod threshold;
pub mod tls;
pub mod types;
pub mod vault;

pub use crate::vault::api::TlsHandshakeCspVault;
pub use crate::vault::local_csp_vault::LocalCspVault;
pub use crate::vault::remote_csp_vault::run_csp_vault_server;
use crate::vault::remote_csp_vault::RemoteCspVault;

use crate::api::{
    CspIDkgProtocol, CspKeyGenerator, CspPublicAndSecretKeyStoreChecker, CspPublicKeyStore,
    CspSigVerifier, CspSigner, CspThresholdEcdsaSigVerifier, CspThresholdEcdsaSigner,
    CspTlsHandshakeSignerProvider, NiDkgCspClient, ThresholdSignatureCspClient,
};
use crate::secret_key_store::SecretKeyStore;
use crate::types::{CspPublicKey, ExternalPublicKeys};
use crate::vault::api::{
    CspPublicKeyStoreError, CspVault, PksAndSksContainsErrors, ValidatePksAndSksError,
};
use ic_adapter_metrics::AdapterMetrics;
use ic_config::crypto::{CryptoConfig, CspVaultType};
use ic_crypto_internal_logmon::metrics::CryptoMetrics;
use ic_crypto_node_key_validation::ValidNodePublicKeys;
use ic_logger::{info, new_logger, replica_logger::no_op_logger, ReplicaLogger};
use ic_types::crypto::CurrentNodePublicKeys;
use key_id::KeyId;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[cfg(test)]
mod tests;

/// Describes the interface of the crypto service provider (CSP), e.g. for
/// signing and key generation. The Csp struct implements this trait.
pub trait CryptoServiceProvider:
    CspSigner
    + CspSigVerifier
    + CspKeyGenerator
    + ThresholdSignatureCspClient
    + NiDkgCspClient
    + CspIDkgProtocol
    + CspThresholdEcdsaSigner
    + CspThresholdEcdsaSigVerifier
    + CspPublicAndSecretKeyStoreChecker
    + CspTlsHandshakeSignerProvider
    + CspPublicKeyStore
{
}

impl<T> CryptoServiceProvider for T where
    T: CspSigner
        + CspSigVerifier
        + CspKeyGenerator
        + ThresholdSignatureCspClient
        + CspIDkgProtocol
        + CspThresholdEcdsaSigner
        + CspThresholdEcdsaSigVerifier
        + NiDkgCspClient
        + CspPublicAndSecretKeyStoreChecker
        + CspTlsHandshakeSignerProvider
        + CspPublicKeyStore
{
}

/// Implements `CryptoServiceProvider` that uses a `CspVault` for
/// storing and managing secret keys.
pub struct Csp {
    csp_vault: Arc<dyn CspVault>,
    logger: ReplicaLogger,
    metrics: Arc<CryptoMetrics>,
}

/// This lock provides the option to add metrics about lock acquisition times.
struct CspRwLock<T> {
    name: String,
    rw_lock: RwLock<T>,
    metrics: Arc<CryptoMetrics>,
}

impl<T> CspRwLock<T> {
    pub fn new_for_rng(content: T, metrics: Arc<CryptoMetrics>) -> Self {
        // Note: The name will appear on metric dashboards and may be used in alerts, do
        // not change this unless you are also updating the monitoring.
        Self::new(content, "csprng".to_string(), metrics)
    }

    pub fn new_for_sks(content: T, metrics: Arc<CryptoMetrics>) -> Self {
        // Note: The name will appear on metric dashboards and may be used in alerts, do
        // not change this unless you are also updating the monitoring.
        Self::new(content, "secret_key_store".to_string(), metrics)
    }

    pub fn new_for_csks(content: T, metrics: Arc<CryptoMetrics>) -> Self {
        // Note: The name will appear on metric dashboards and may be used in alerts, do
        // not change this unless you are also updating the monitoring.
        Self::new(content, "canister_secret_key_store".to_string(), metrics)
    }

    pub fn new_for_public_key_store(content: T, metrics: Arc<CryptoMetrics>) -> Self {
        // Note: The name will appear on metric dashboards and may be used in alerts, do
        // not change this unless you are also updating the monitoring.
        Self::new(content, "public_key_store".to_string(), metrics)
    }

    fn new(content: T, lock_name: String, metrics: Arc<CryptoMetrics>) -> Self {
        Self {
            name: lock_name,
            rw_lock: RwLock::new(content),
            metrics,
        }
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let start_time = self.metrics.now();
        let write_guard = self.rw_lock.write();
        self.observe(&self.metrics, "write", start_time);
        write_guard
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let start_time = self.metrics.now();
        let read_guard = self.rw_lock.read();
        self.observe(&self.metrics, "read", start_time);
        read_guard
    }

    fn observe(&self, metrics: &CryptoMetrics, access: &str, start_time: Option<Instant>) {
        metrics.observe_lock_acquisition_duration_seconds(&self.name, access, start_time);
    }
}

impl Csp {
    /// Creates a production-grade crypto service provider.
    ///
    /// If the `config`'s vault type is `UnixSocket`, a `tokio_runtime_handle`
    /// must be provided, which is then used for the `async`hronous
    /// communication with the vault via RPC.
    ///
    /// # Panics
    /// Panics if the `config`'s vault type is `UnixSocket` and
    /// `tokio_runtime_handle` is `None`.
    pub fn new(
        config: &CryptoConfig,
        tokio_runtime_handle: Option<tokio::runtime::Handle>,
        logger: Option<ReplicaLogger>,
        metrics: Arc<CryptoMetrics>,
    ) -> Self {
        match &config.csp_vault_type {
            CspVaultType::InReplica => Self::new_with_in_replica_vault(config, logger, metrics),
            CspVaultType::UnixSocket {
                logic: logic_socket_path,
                metrics: metrics_socket_path,
            } => Self::new_with_unix_socket_vault(
                logic_socket_path,
                metrics_socket_path.clone(),
                tokio_runtime_handle.expect("missing tokio runtime handle"),
                config,
                logger,
                metrics,
            ),
        }
    }

    fn new_with_in_replica_vault(
        config: &CryptoConfig,
        logger: Option<ReplicaLogger>,
        metrics: Arc<CryptoMetrics>,
    ) -> Self {
        let logger = logger.unwrap_or_else(no_op_logger);
        info!(
            logger,
            "Proceeding with an in-replica csp_vault, CryptoConfig: {:?}", config
        );
        let csp_vault =
            LocalCspVault::new_in_dir(&config.crypto_root, metrics.clone(), new_logger!(&logger));
        Csp::builder(csp_vault, logger, metrics).build()
    }

    fn new_with_unix_socket_vault(
        socket_path: &Path,
        metrics_socket_path: Option<PathBuf>,
        rt_handle: tokio::runtime::Handle,
        config: &CryptoConfig,
        logger: Option<ReplicaLogger>,
        metrics: Arc<CryptoMetrics>,
    ) -> Self {
        let logger = logger.unwrap_or_else(no_op_logger);
        info!(
            logger,
            "Proceeding with a remote csp_vault, CryptoConfig: {:?}", config
        );
        if let (Some(metrics_uds_path), Some(global_metrics)) =
            (metrics_socket_path, metrics.metrics_registry())
        {
            global_metrics.register_adapter(AdapterMetrics::new(
                "cryptocsp",
                metrics_uds_path,
                rt_handle.clone(),
            ));
        }

        let csp_vault = RemoteCspVault::new(
            socket_path,
            rt_handle,
            new_logger!(&logger),
            metrics.clone(),
        )
        .unwrap_or_else(|e| {
            panic!(
                "Could not connect to CspVault at socket {:?}: {:?}",
                socket_path, e
            )
        });
        Csp::builder(csp_vault, logger, metrics).build()
    }
}

impl CspPublicKeyStore for Csp {
    fn current_node_public_keys(&self) -> Result<CurrentNodePublicKeys, CspPublicKeyStoreError> {
        self.csp_vault.current_node_public_keys()
    }

    fn current_node_public_keys_with_timestamps(
        &self,
    ) -> Result<CurrentNodePublicKeys, CspPublicKeyStoreError> {
        self.csp_vault.current_node_public_keys_with_timestamps()
    }

    fn idkg_dealing_encryption_pubkeys_count(&self) -> Result<usize, CspPublicKeyStoreError> {
        self.csp_vault.idkg_dealing_encryption_pubkeys_count()
    }
}

impl CspPublicAndSecretKeyStoreChecker for Csp {
    fn pks_and_sks_contains(
        &self,
        external_public_keys: ExternalPublicKeys,
    ) -> Result<(), PksAndSksContainsErrors> {
        self.csp_vault.pks_and_sks_contains(external_public_keys)
    }

    fn validate_pks_and_sks(&self) -> Result<ValidNodePublicKeys, ValidatePksAndSksError> {
        self.csp_vault.validate_pks_and_sks()
    }
}
