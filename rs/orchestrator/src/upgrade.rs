use crate::catch_up_package_provider::CatchUpPackageProvider;
use crate::error::{OrchestratorError, OrchestratorResult};
use crate::metrics::OrchestratorMetrics;
use crate::registry_helper::RegistryHelper;
use crate::replica_process::ReplicaProcess;
use async_trait::async_trait;
use ic_http_utils::file_downloader::FileDownloader;
use ic_image_upgrader::error::{UpgradeError, UpgradeResult};
use ic_image_upgrader::ImageUpgrader;
use ic_interfaces_registry::RegistryClient;
use ic_logger::{info, warn, ReplicaLogger};
use ic_registry_client_helpers::node::NodeRegistry;
use ic_registry_client_helpers::subnet::SubnetRegistry;
use ic_registry_local_store::LocalStoreImpl;
use ic_registry_replicator::RegistryReplicator;
use ic_types::consensus::{CatchUpPackage, HasHeight};
use ic_types::{Height, NodeId, RegistryVersion, ReplicaVersion, SubnetId};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Provides function to continuously check the Registry to determine if this
/// node should upgrade to a new release package, and if so, downloads and
/// extracts this release package and exec's the orchestrator binary contained
/// within.
pub(crate) struct Upgrade {
    pub registry: Arc<RegistryHelper>,
    replica_process: Arc<Mutex<ReplicaProcess>>,
    cup_provider: Arc<CatchUpPackageProvider>,
    replica_version: ReplicaVersion,
    replica_config_file: PathBuf,
    pub ic_binary_dir: PathBuf,
    pub image_path: PathBuf,
    registry_replicator: Arc<RegistryReplicator>,
    pub logger: ReplicaLogger,
    node_id: NodeId,
    /// The replica version that is prepared by 'prepare_upgrade' to upgrade to.
    pub prepared_upgrade_version: Option<ReplicaVersion>,
    pub orchestrator_data_directory: Option<PathBuf>,
}

impl Upgrade {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        registry: Arc<RegistryHelper>,
        metrics: Arc<OrchestratorMetrics>,
        replica_process: Arc<Mutex<ReplicaProcess>>,
        cup_provider: Arc<CatchUpPackageProvider>,
        replica_version: ReplicaVersion,
        replica_config_file: PathBuf,
        node_id: NodeId,
        ic_binary_dir: PathBuf,
        registry_replicator: Arc<RegistryReplicator>,
        release_content_dir: PathBuf,
        logger: ReplicaLogger,
        orchestrator_data_directory: Option<PathBuf>,
    ) -> Self {
        let value = Self {
            registry,
            replica_process,
            cup_provider,
            node_id,
            replica_version,
            replica_config_file,
            ic_binary_dir,
            image_path: release_content_dir.join("image.bin"),
            registry_replicator,
            logger: logger.clone(),
            prepared_upgrade_version: None,
            orchestrator_data_directory,
        };
        if let Err(e) = value.report_reboot_time(metrics) {
            warn!(logger, "Cannot report the reboot time: {}", e);
        }
        value.confirm_boot().await;
        value
    }

    fn report_reboot_time(&self, metrics: Arc<OrchestratorMetrics>) -> OrchestratorResult<()> {
        let elapsed_time = self.get_time_since_last_reboot_trigger()?;
        metrics.reboot_duration.set(elapsed_time.as_secs() as i64);
        Ok(())
    }

    /// Checks for a new release package, and if found, upgrades to this release
    /// package
    pub(crate) async fn check(&mut self) -> OrchestratorResult<Option<SubnetId>> {
        let latest_registry_version = self.registry.get_latest_version();
        // Determine the subnet_id using the local CUP.
        let (subnet_id, local_cup) = if let Some(cup) = self.cup_provider.get_local_cup() {
            let subnet_id =
                get_subnet_id(&*self.registry.registry_client, &cup.cup).map_err(|err| {
                    OrchestratorError::UpgradeError(format!(
                        "Couldn't determine the subnet id: {:?}",
                        err
                    ))
                })?;
            (subnet_id, Some(cup))
        } else {
            // No local CUP found, check registry
            match self.registry.get_subnet_id(latest_registry_version) {
                Ok(subnet_id) => {
                    info!(self.logger, "Assignment to subnet {} detected", subnet_id);
                    (subnet_id, None)
                }
                // If no subnet is assigned to the node id, we're unassigned.
                _ => {
                    self.check_for_upgrade_as_unassigned().await?;
                    return Ok(None);
                }
            }
        };

        // When we arrived here, we are an assigned node.
        let old_cup_height = local_cup.as_ref().map(|cup| cup.cup.content.height());

        // Get the latest available CUP from the disk, peers or registry and
        // persist it if necesasry.
        let cup = self
            .cup_provider
            .get_latest_cup(local_cup, subnet_id)
            .await?;

        // If the CUP is unsigned, it's a registry CUP and we're in a genesis or subnet
        // recovery scenario. Check if we're in an NNS subnet recovery case and download
        // the new registry if needed.
        if cup.cup.signature.signature.get_ref().0.is_empty() {
            info!(
                self.logger,
                "The latest CUP (registry version={}, height={}) is unsigned: a subnet genesis/recovery is in progress",
                cup.cup.content.registry_version(),
                cup.cup.height(),
            );
            self.download_registry_and_restart_if_nns_subnet_recovery(
                subnet_id,
                latest_registry_version,
            )
            .await?;
        }

        // Now when we have the most recent CUP, we check if we're still assigned.
        // If not, go into unassigned state.
        if should_node_become_unassigned(
            &*self.registry.registry_client,
            self.node_id,
            subnet_id,
            &cup.cup,
        ) {
            self.stop_replica()?;
            remove_node_state(
                self.replica_config_file.clone(),
                self.cup_provider.get_cup_path(),
            )
            .map_err(OrchestratorError::UpgradeError)?;
            info!(self.logger, "Subnet state removed");
            return Ok(None);
        }

        // If we arrived here, we have the newest CUP and we're still assigned.
        // Now we check if this CUP requires a new replica version.
        let cup_registry_version = cup.cup.content.registry_version();
        let new_replica_version = self
            .registry
            .get_replica_version(subnet_id, cup_registry_version)?;
        if new_replica_version != self.replica_version {
            info!(
                self.logger,
                "Starting version upgrade at CUP registry version {}: {} -> {}",
                cup_registry_version,
                self.replica_version,
                new_replica_version
            );
            // Only downloads the new image if it doesn't already exists locally, i.e. it
            // was previously downloaded by `prepare_upgrade_if_scheduled()`, see
            // below.
            return self
                .execute_upgrade(&new_replica_version)
                .await
                .map_err(OrchestratorError::from);
        }

        // If we arrive here, we are on the newest replica version.
        // Now we check if a subnet recovery is in progress.
        // If it is, we restart to pass the unsigned CUP to consensus.
        self.stop_replica_if_new_recovery_cup(&cup.cup, old_cup_height);

        // This will start a new replica process if none is running.
        self.ensure_replica_is_running(&self.replica_version, subnet_id)?;

        // This will trigger an image download if one is already scheduled but we did
        // not arrive at the corresponding CUP yet.
        self.prepare_upgrade_if_scheduled(subnet_id).await?;

        Ok(Some(subnet_id))
    }

    // Special case for when we are doing boostrap subnet recovery for
    // nns and replacing the local registry store. Because we replace the
    // contents of the local registry store in the process of doing this, we
    // will not perpetually hit this case, and thus it is not important to
    // check the height.
    async fn download_registry_and_restart_if_nns_subnet_recovery(
        &self,
        subnet_id: SubnetId,
        registry_version: RegistryVersion,
    ) -> OrchestratorResult<()> {
        if let Some(registry_contents) = self
            .registry
            .registry_client
            .get_cup_contents(subnet_id, registry_version)
            .ok()
            .and_then(|record| record.value)
        {
            if let Some(registry_store_uri) = registry_contents.registry_store_uri {
                warn!(
                    self.logger,
                    "Downloading registry data from {} with hash {} for subnet recovery",
                    registry_store_uri.uri,
                    registry_store_uri.hash,
                );
                let downloader = FileDownloader::new(Some(self.logger.clone()));
                let local_store_location = tempfile::tempdir()
                    .expect("temporary location for local store download could not be created")
                    .into_path();
                downloader
                    .download_and_extract_tar_gz(
                        &registry_store_uri.uri,
                        &local_store_location,
                        Some(registry_store_uri.hash),
                    )
                    .await
                    .map_err(OrchestratorError::FileDownloadError)?;
                if let Err(e) = self.stop_replica() {
                    // Even though we fail to stop the replica, we should still
                    // replace the registry local store, so we simply issue a warning.
                    warn!(self.logger, "Failed to stop replica with error {:?}", e);
                }
                let new_local_store = LocalStoreImpl::new(local_store_location);
                self.registry_replicator
                    .stop_polling_and_set_local_registry_data(&new_local_store);
                reexec_current_process(&self.logger);
            }
        }
        Ok(())
    }

    // Checks if the subnet record for the given subnet_id contains a different
    // replica version. If it is the case, the image will be downloaded. This
    // allows us to decrease the upgrade downtime.
    async fn prepare_upgrade_if_scheduled(
        &mut self,
        subnet_id: SubnetId,
    ) -> OrchestratorResult<()> {
        let (expected_replica_version, registry_version) =
            self.registry.get_expected_replica_version(subnet_id)?;
        if expected_replica_version != self.replica_version {
            info!(
                self.logger,
                "Replica version upgrade detected at registry version {}: {} -> {}",
                registry_version,
                self.replica_version,
                expected_replica_version
            );
            self.prepare_upgrade(&expected_replica_version).await?
        }
        Ok(())
    }

    async fn check_for_upgrade_as_unassigned(&mut self) -> OrchestratorResult<()> {
        let registry_version = self.registry.get_latest_version();
        let replica_version = self
            .registry
            .get_unassigned_replica_version(registry_version)?;
        if self.replica_version == replica_version {
            return Ok(());
        }
        info!(
            self.logger,
            "Replica upgrade on unassigned node detected: old version {}, new version {}",
            self.replica_version,
            replica_version
        );
        self.execute_upgrade(&replica_version)
            .await
            .map_err(OrchestratorError::from)
    }

    /// Stop the current replica process.
    pub fn stop_replica(&self) -> OrchestratorResult<()> {
        self.replica_process.lock().unwrap().stop().map_err(|e| {
            OrchestratorError::IoError(
                "Error when attempting to stop replica during upgrade".into(),
                e,
            )
        })
    }

    // Stop the replica if the given CUP is unsigned and higher than the given height.
    // Without restart, consensus would reject the unsigned artifact.
    // If stopping the replica fails, restart the current process instead.
    fn stop_replica_if_new_recovery_cup(
        &self,
        cup: &CatchUpPackage,
        old_cup_height: Option<Height>,
    ) {
        let is_unsigned_cup = cup.signature.signature.get_ref().0.is_empty();
        let new_height = cup.content.height();
        if is_unsigned_cup && old_cup_height.is_some() && Some(new_height) > old_cup_height {
            info!(
                self.logger,
                "Found higher unsigned CUP, restarting replica for subnet recovery..."
            );
            // Restarting the replica is enough to pass the unsigned CUP forward.
            // If we fail, restart the current process instead.
            if let Err(e) = self.stop_replica() {
                warn!(self.logger, "Failed to stop replica with error {:?}", e);
                reexec_current_process(&self.logger);
            }
        }
    }

    // Start the replica process if not running already
    fn ensure_replica_is_running(
        &self,
        replica_version: &ReplicaVersion,
        subnet_id: SubnetId,
    ) -> OrchestratorResult<()> {
        if self.replica_process.lock().unwrap().is_running() {
            return Ok(());
        }
        info!(self.logger, "Starting new replica process");
        let cup_path = self.cup_provider.get_cup_path();
        let replica_binary = self
            .ic_binary_dir
            .join("replica")
            .as_path()
            .display()
            .to_string();
        let cmd = vec![
            format!("--replica-version={}", replica_version.as_ref()),
            format!(
                "--config-file={}",
                self.replica_config_file.as_path().display()
            ),
            format!("--catch-up-package={}", cup_path.as_path().display()),
            format!("--force-subnet={}", subnet_id),
        ];

        self.replica_process
            .lock()
            .unwrap()
            .start(replica_binary, replica_version, cmd)
            .map_err(|e| {
                OrchestratorError::IoError("Error when attempting to start new replica".into(), e)
            })
    }
}

#[async_trait]
impl ImageUpgrader<ReplicaVersion, Option<SubnetId>> for Upgrade {
    fn get_prepared_version(&self) -> Option<&ReplicaVersion> {
        self.prepared_upgrade_version.as_ref()
    }

    fn set_prepared_version(&mut self, version: Option<ReplicaVersion>) {
        self.prepared_upgrade_version = version
    }

    fn binary_dir(&self) -> &PathBuf {
        &self.ic_binary_dir
    }

    fn image_path(&self) -> &PathBuf {
        &self.image_path
    }

    fn data_dir(&self) -> &Option<PathBuf> {
        &self.orchestrator_data_directory
    }

    fn get_release_package_urls_and_hash(
        &self,
        version: &ReplicaVersion,
    ) -> UpgradeResult<(Vec<String>, Option<String>)> {
        let mut record = self
            .registry
            .get_replica_version_record(version.clone(), self.registry.get_latest_version())
            .map_err(UpgradeError::from)?;

        // OR-253 shall remove this statement along with `release_package_url`.
        // Until then, we need to remain compatible with older replica version records
        // that contain only a single URL in `release_package_url`. This would duplicate
        // the first URL in newer blessed versions, which is temporarily accepted.
        record
            .release_package_urls
            .push(record.release_package_url.clone());

        Ok((
            record.release_package_urls,
            Some(record.release_package_sha256_hex),
        ))
    }

    fn log(&self) -> &ReplicaLogger {
        &self.logger
    }

    fn get_load_balance_number(&self) -> usize {
        // XOR all the u8 in node_id:
        let principal = self.node_id.get().0;
        principal.as_slice().iter().fold(0, |acc, x| (acc ^ x)) as usize
    }

    async fn check_for_upgrade(&mut self) -> UpgradeResult<Option<SubnetId>> {
        self.check().await.map_err(UpgradeError::from)
    }
}

// Returns the subnet id for the given CUP.
fn get_subnet_id(registry: &dyn RegistryClient, cup: &CatchUpPackage) -> Result<SubnetId, String> {
    let dkg_summary = &cup
        .content
        .block
        .get_value()
        .payload
        .as_ref()
        .as_summary()
        .dkg;
    // Note that although sometimes CUPs have no signatures (e.g. genesis and
    // recovery CUPs) they always have the signer id (the DKG id), which is taken
    // from the high-threshold transcript when we build a genesis/recovery CUP.
    let dkg_id = cup.signature.signer;
    use ic_types::crypto::threshold_sig::ni_dkg::NiDkgTargetSubnet;
    // If the DKG key material was signed by the subnet itself — use it, if not, get
    // the subnet id from the registry.
    match dkg_id.target_subnet {
        NiDkgTargetSubnet::Local => Ok(dkg_id.dealer_subnet),
        // If we hit this case, then the local CUP is a genesis or recovery CUP of an application
        // subnet or of the NNS subnet recovered on failover nodes. We cannot derive the subnet id
        // from it, so we use the registry version of that CUP and the node id of one of the
        // high-threshold committee members, to find out to which subnet this node belongs to.
        NiDkgTargetSubnet::Remote(_) => {
            let node_id = dkg_summary
                .current_transcripts()
                .values()
                .next()
                .ok_or("No current transcript found")?
                .committee
                .get()
                .iter()
                .next()
                .ok_or("No nodes in current transcript committee found")?;
            match registry.get_subnet_id_from_node_id(*node_id, dkg_summary.registry_version) {
                Ok(Some(subnet_id)) => Ok(subnet_id),
                other => Err(format!(
                    "Couldn't get the subnet id from the registry for node {:?} at registry version {}: {:?}",
                    node_id, dkg_summary.registry_version, other
                )),
            }
        }
    }
}

// Checks if the node still belongs to the subnet it was assigned the last time.
// We decide this by checking the subnet membership starting from the oldest
// relevant version of the local CUP and ending with the latest registry
// version.
fn should_node_become_unassigned(
    registry: &dyn RegistryClient,
    node_id: NodeId,
    subnet_id: SubnetId,
    cup: &CatchUpPackage,
) -> bool {
    let summary = &cup.content.block.get_value().payload.as_ref().as_summary();
    let oldest_relevant_version = summary.get_oldest_registry_version_in_use().get();
    let latest_registry_version = registry.get_latest_version().get();
    // Make sure that if the latest registry version is for some reason violating
    // the assumption that it's higher/equal than any other version used in the
    // system, we still do not remove the subnet state by a mistake.
    if latest_registry_version < oldest_relevant_version {
        return false;
    }
    for version in oldest_relevant_version..=latest_registry_version {
        if let Ok(Some(members)) =
            registry.get_node_ids_on_subnet(subnet_id, RegistryVersion::from(version))
        {
            if members.iter().any(|id| id == &node_id) {
                return false;
            }
        }
    }
    true
}

// Deletes the subnet state consisting of the consensus pool, execution state
// and the local CUP.
fn remove_node_state(replica_config_file: PathBuf, cup_path: PathBuf) -> Result<(), String> {
    use ic_config::{Config, ConfigSource};
    use std::fs::{remove_dir_all, remove_file};
    let tmpdir = tempfile::Builder::new()
        .prefix("ic_config")
        .tempdir()
        .map_err(|err| format!("Couldn't create a temporary directory: {:?}", err))?;
    let config = Config::load_with_tmpdir(
        ConfigSource::File(replica_config_file),
        tmpdir.path().to_path_buf(),
    );

    let consensus_pool_path = config.artifact_pool.consensus_pool_path;
    remove_dir_all(&consensus_pool_path).map_err(|err| {
        format!(
            "Couldn't delete the consensus pool at {:?}: {:?}",
            consensus_pool_path, err
        )
    })?;

    let state_path = config.state_manager.state_root();
    remove_dir_all(&state_path)
        .map_err(|err| format!("Couldn't delete the state at {:?}: {:?}", state_path, err))?;

    remove_file(&cup_path)
        .map_err(|err| format!("Couldn't delete the CUP at {:?}: {:?}", cup_path, err))?;

    Ok(())
}

// Re-execute the current process, exactly as it was originally called.
fn reexec_current_process(logger: &ReplicaLogger) -> OrchestratorError {
    let args: Vec<String> = std::env::args().collect();
    info!(
        logger,
        "Restarting the current process with the same arguments it was originally executed with: {:?}",
        &args[..]
    );
    let error = exec::Command::new(&args[0]).args(&args[1..]).exec();
    OrchestratorError::ExecError(PathBuf::new(), error)
}
