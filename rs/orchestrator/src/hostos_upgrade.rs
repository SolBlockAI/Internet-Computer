use std::sync::Arc;
use std::time::Duration;

use crate::error::{OrchestratorError, OrchestratorResult};
use crate::registry_helper::RegistryHelper;
use ic_logger::{info, warn, ReplicaLogger};
use ic_protobuf::registry::hostos_version::v1::HostosVersionRecord;
use ic_sys::utility_command::UtilityCommand;
use ic_types::hostos_version::HostosVersion;
use ic_types::NodeId;

use tokio::sync::watch::Receiver;

pub(crate) struct HostosUpgrader {
    registry: Arc<RegistryHelper>,
    hostos_version: HostosVersion,
    node_id: NodeId,
    logger: ReplicaLogger,
}

impl HostosUpgrader {
    pub(crate) async fn new(
        registry: Arc<RegistryHelper>,
        hostos_version: HostosVersion,
        node_id: NodeId,
        logger: ReplicaLogger,
    ) -> Self {
        Self {
            registry,
            hostos_version,
            node_id,
            logger,
        }
    }
}

impl HostosUpgrader {
    /// Calls `check_for_upgrade()` once every `interval`, timing out after `timeout`.
    /// Awaiting this function blocks until `exit_signal` is set to `true`.
    /// For every execution of `check_for_upgrade()` the given handler is called with
    /// the result returned by the check.
    pub async fn upgrade_loop(
        &mut self,
        mut exit_signal: Receiver<bool>,
        interval: Duration,
        timeout: Duration,
    ) {
        // Wait for a minute before starting the loop, to allow the registry
        // some time to catch up, after starting.
        tokio::time::sleep(Duration::from_secs(60)).await;
        while !*exit_signal.borrow() {
            if let Err(e) = tokio::time::timeout(timeout, self.check_for_upgrade()).await {
                warn!(&self.logger, "Check for upgrade failed: {:?}", e);
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = exit_signal.changed() => {}
            };
        }
    }

    async fn check_for_upgrade(&mut self) -> OrchestratorResult<()> {
        let latest_registry_version = self.registry.get_latest_version();

        let node_id = self.node_id;

        let node_hostos_version = self
            .registry
            .get_node_hostos_version(latest_registry_version)?;

        if let Some(node_hostos_version) = node_hostos_version {
            if self.hostos_version != node_hostos_version {
                info!(
                    self.logger,
                    "Found HostOS version '{node_hostos_version}' set for this node '{node_id}'",
                );
                info!(
                    self.logger,
                    "Starting HostOS upgrade at registry version {}: {} -> {}",
                    latest_registry_version,
                    self.hostos_version,
                    node_hostos_version
                );
                return self.execute_upgrade(&node_hostos_version).await;
            }
        }

        Ok(())
    }

    async fn execute_upgrade(&mut self, version: &HostosVersion) -> OrchestratorResult<()> {
        let hostos_version_record = self
            .registry
            .get_hostos_version_record(version.clone(), self.registry.get_latest_version())?;

        let HostosVersionRecord {
            mut release_package_urls,
            release_package_sha256_hex: hash,
            ..
        } = hostos_version_record;

        // Load-balance, by making each node rotate the `release_package_urls` by some number.
        // Note that the order is the same for everyone; only the starting point is different.
        // This is okay because we do expect the first attempt to be successful.
        let url_count = release_package_urls.len();
        release_package_urls.rotate_right(self.get_load_balance_number() % url_count);

        let mut error = format!("No download URLs are provided for version {:?}", version);

        for release_package_url in release_package_urls.iter() {
            // We only ever expect this command to exit in error. If the
            // upgrade call succeeds, the HostOS will reboot and shut us down.
            if let Err(e) = UtilityCommand::request_hostos_upgrade(release_package_url, &hash) {
                info!(
                    &self.logger,
                    "HostOS upgrade failed using: '{release_package_url}'"
                );
                error = e;
            }
        }

        Err(OrchestratorError::UpgradeError(error))
    }

    fn get_load_balance_number(&self) -> usize {
        // XOR all the u8 in node_id:
        let principal = self.node_id.get().0;
        principal.as_slice().iter().fold(0, |acc, x| (acc ^ x)) as usize
    }
}
