//! Linux CSI node service. Axo supplies preformatted devices; this module never
//! formats a disk and never talks to the cloud or to Ceph.
mod host;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use crate::csi;
use host::{Device, Host, LinuxHost, Mount};

#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Exact, incarnation-specific VM identity provided by the platform.
    /// Never populate this with a Kubernetes Node name or an IP address.
    pub node_id: String,
    pub state_dir: PathBuf,
    pub device_root: PathBuf,
    pub sys_block_dir: PathBuf,
    pub kubelet_root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("invalid node configuration: {0}")]
    Configuration(String),
    #[error("node state I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid persisted node state: {0}")]
    State(#[from] serde_json::Error),
    #[error("another node service owns the state directory")]
    Locked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    Filesystem(String),
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Spec {
    mode: Mode,
    flags: Vec<String>,
    access_mode: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Volume {
    serial: String,
    device: Device,
    staging_path: PathBuf,
    spec: Spec,
    published: BTreeMap<PathBuf, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registry {
    version: u32,
    node_id: String,
    volumes: BTreeMap<String, Volume>,
}

pub struct NodeService {
    config: NodeConfig,
    // Serialize operations, including the device/mount checks, with their effects.
    // Persist intent before each effect so cancellation/crash is retryable.
    registry: Mutex<Registry>,
    host: Arc<dyn Host>,
    _process_lock: Flock<File>,
}

fn validate_token(value: &str, field: &str, max: usize) -> Result<(), Status> {
    if value.is_empty()
        || value.len() > max
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(Status::invalid_argument(format!(
            "{field} must be nonempty, bounded, and contain no whitespace/control characters"
        )));
    }
    Ok(())
}

fn validate_volume_id(value: &str) -> Result<(), Status> {
    // CSI handles are opaque JSON keys, never filenames or command arguments.
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(Status::invalid_argument(
            "volume_id must be nonempty, at most 1024 bytes, and contain no control characters",
        ));
    }
    Ok(())
}

fn absolute_clean(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().is_empty()
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

/// Reject symlinks in every existing component, including the leaf. CSI callers
/// may name a not-yet-created target, but may not redirect operations elsewhere.
fn no_symlinks(path: &Path) -> Result<(), Status> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Status::invalid_argument("volume path contains a symlink"));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(Status::internal(format!("inspect volume path: {error}"))),
        }
    }
    Ok(())
}

fn target_path(value: &str, root: &Path) -> Result<PathBuf, Status> {
    let path = PathBuf::from(value);
    if !absolute_clean(&path) || path == root || !path.starts_with(root) {
        return Err(Status::invalid_argument(
            "volume path must be a clean absolute descendant of kubelet_root",
        ));
    }
    no_symlinks(&path)?;
    Ok(path)
}

fn flags(mount_flags: &[String], block: bool, readonly: bool) -> Result<Vec<String>, Status> {
    let allowed = [
        "ro",
        "rw",
        "nosuid",
        "nodev",
        "noexec",
        "noatime",
        "relatime",
        "strictatime",
        "nodiratime",
        "sync",
        "dirsync",
        "discard",
        "nodiscard",
        "nouuid",
    ];
    let mut result = BTreeSet::new();
    for flag in mount_flags {
        if !allowed.contains(&flag.as_str()) {
            return Err(Status::invalid_argument(format!(
                "unsupported mount flag: {flag}"
            )));
        }
        result.insert(flag.clone());
    }
    for incompatible in [
        ["ro", "rw"],
        ["discard", "nodiscard"],
        ["noatime", "relatime"],
        ["noatime", "strictatime"],
        ["relatime", "strictatime"],
    ] {
        if incompatible.iter().all(|flag| result.contains(*flag)) {
            return Err(Status::invalid_argument("conflicting mount flags"));
        }
    }
    result.insert("nosuid".to_owned());
    if !block {
        result.insert("nodev".to_owned());
    }
    if readonly {
        result.remove("rw");
        result.insert("ro".to_owned());
    }
    if !result.contains("ro") {
        result.insert("rw".to_owned());
    }
    Ok(result.into_iter().collect())
}

fn spec(
    capability: Option<&csi::VolumeCapability>,
    context: &HashMap<String, String>,
) -> Result<Spec, Status> {
    let capability =
        capability.ok_or_else(|| Status::invalid_argument("volume_capability is required"))?;
    let access = capability
        .access_mode
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("access_mode is required"))?
        .mode;
    // Match the controller contract: RWO only, no shared/multi-node mounting.
    if ![1, 2].contains(&access) {
        return Err(Status::invalid_argument(
            "only SINGLE_NODE_WRITER and SINGLE_NODE_READER_ONLY are supported",
        ));
    }
    if context.get("format_policy").map(String::as_str) != Some("preformatted") {
        return Err(Status::failed_precondition(
            "Axo must provide format_policy=preformatted; node formatting is forbidden",
        ));
    }
    let readonly = match context.get("readonly").map(String::as_str) {
        Some("true") => true,
        Some("false") | None => false,
        _ => {
            return Err(Status::invalid_argument(
                "publish_context.readonly must be true or false",
            ));
        }
    };
    if access == 2 && !readonly {
        return Err(Status::invalid_argument(
            "SINGLE_NODE_READER_ONLY requires a read-only attachment",
        ));
    }
    match &capability.access_type {
        Some(csi::volume_capability::AccessType::Mount(mount)) => {
            let fs_type = context
                .get("fs_type")
                .ok_or_else(|| Status::invalid_argument("publish_context.fs_type is required"))?;
            if !["xfs", "ext4"].contains(&fs_type.as_str()) {
                return Err(Status::invalid_argument("only xfs and ext4 are supported"));
            }
            if context.get("volume_mode").map(String::as_str) != Some("filesystem")
                || (!mount.fs_type.is_empty() && mount.fs_type != *fs_type)
            {
                return Err(Status::invalid_argument(
                    "filesystem capability differs from publish_context",
                ));
            }
            if !mount.volume_mount_group.is_empty() {
                return Err(Status::invalid_argument(
                    "CSI volume_mount_group is not supported",
                ));
            }
            let mut mount_flags = mount.mount_flags.clone();
            if fs_type == "xfs" {
                // Snapshots and clones retain XFS UUIDs. Device serials, not
                // filesystem UUIDs, establish volume identity and exclusivity.
                if !mount_flags.iter().any(|flag| flag == "nouuid") {
                    mount_flags.push("nouuid".into());
                }
            } else if mount_flags.iter().any(|flag| flag == "nouuid") {
                return Err(Status::invalid_argument("nouuid is valid only for XFS"));
            }
            Ok(Spec {
                mode: Mode::Filesystem(fs_type.clone()),
                flags: flags(&mount_flags, false, readonly)?,
                access_mode: access,
            })
        }
        Some(csi::volume_capability::AccessType::Block(_)) => {
            if context.get("volume_mode").map(String::as_str) != Some("block")
                || context.contains_key("fs_type")
            {
                return Err(Status::invalid_argument(
                    "block capability differs from publish_context",
                ));
            }
            Ok(Spec {
                mode: Mode::Block,
                flags: flags(&[], true, readonly)?,
                access_mode: access,
            })
        }
        None => Err(Status::invalid_argument(
            "mount or block access_type is required",
        )),
    }
}

fn unique_mount<'a>(mounts: &'a [Mount], target: &Path) -> Result<Option<&'a Mount>, Status> {
    let matches: Vec<_> = mounts
        .iter()
        .filter(|mount| mount.target == target)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(Status::failed_precondition(
            "stacked mounts at target; refusing to modify them",
        )),
    }
}

impl NodeService {
    pub fn new(mut config: NodeConfig) -> Result<Self, NodeError> {
        validate_token(&config.node_id, "node_id (platform VM identity)", 256)
            .map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
        for path in [
            &config.state_dir,
            &config.device_root,
            &config.sys_block_dir,
            &config.kubelet_root,
        ] {
            if !absolute_clean(path) || path == Path::new("/") {
                return Err(NodeError::Configuration(
                    "configured directories must be clean absolute non-root paths".to_owned(),
                ));
            }
            no_symlinks(path).map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
        }
        std::fs::create_dir_all(&config.state_dir)?;
        std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700))?;
        config.device_root = std::fs::canonicalize(&config.device_root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(config.state_dir.join("node.lock"))?;
        let lock =
            Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| NodeError::Locked)?;
        let state_path = config.state_dir.join("registry.json");
        no_symlinks(&state_path).map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
        let registry: Registry = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry {
                version: 1,
                node_id: config.node_id.clone(),
                volumes: BTreeMap::new(),
            },
            Err(error) => return Err(error.into()),
        };
        if registry.version != 1 || registry.node_id != config.node_id {
            return Err(NodeError::Configuration(
                "persisted state belongs to another VM incarnation or schema version".to_owned(),
            ));
        }
        for (id, volume) in &registry.volumes {
            validate_volume_id(id).map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
            validate_token(&volume.serial, "persisted device_serial", 256)
                .map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
            for path in std::iter::once(&volume.staging_path).chain(volume.published.keys()) {
                target_path(&path.to_string_lossy(), &config.kubelet_root)
                    .map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
            }
        }
        let host = Arc::new(LinuxHost {
            device_root: config.device_root.clone(),
            sys_block_dir: config.sys_block_dir.clone(),
        });
        let service = Self {
            config,
            registry: Mutex::new(registry.clone()),
            host,
            _process_lock: lock,
        };
        service.persist(&registry).map_err(NodeError::Io)?;
        Ok(service)
    }

    fn persist(&self, registry: &Registry) -> std::io::Result<()> {
        let temporary = self.config.state_dir.join("registry.pending");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, registry)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(temporary, self.config.state_dir.join("registry.json"))?;
        File::open(&self.config.state_dir)?.sync_all()
    }

    fn update(&self, current: &mut Registry, next: Registry) -> Result<(), Status> {
        self.persist(&next)
            .map_err(|e| Status::internal(format!("persist node state: {e}")))?;
        *current = next;
        Ok(())
    }

    fn path(&self, value: &str) -> Result<PathBuf, Status> {
        let path = target_path(value, &self.config.kubelet_root)?;
        if path.starts_with(&self.config.state_dir) || self.config.state_dir.starts_with(&path) {
            return Err(Status::invalid_argument(
                "volume target overlaps the driver state directory",
            ));
        }
        Ok(path)
    }

    fn require_unclaimed_path(
        registry: &Registry,
        target: &Path,
        owner: &str,
        existing_stage: bool,
    ) -> Result<(), Status> {
        for (id, volume) in &registry.volumes {
            for path in std::iter::once(&volume.staging_path).chain(volume.published.keys()) {
                if id == owner
                    && path == target
                    && (existing_stage || volume.published.contains_key(path))
                {
                    continue;
                }
                if path.starts_with(target) || target.starts_with(path) {
                    return Err(Status::already_exists(
                        "path overlaps a registered volume path",
                    ));
                }
            }
        }
        Ok(())
    }

    async fn current_device(&self, volume: &Volume) -> Result<Device, Status> {
        let device = self.host.discover(&volume.serial).await?;
        if device != volume.device {
            return Err(Status::failed_precondition(
                "device identity changed since staging; unstage before reuse",
            ));
        }
        Ok(device)
    }

    async fn device_for_cleanup(&self, volume: &Volume) -> Result<(), Status> {
        match self.current_device(volume).await {
            Ok(_) => Ok(()),
            // Hot-unplug can remove sysfs before kubelet requests cleanup. The
            // caller must still match the recorded mount identity; any present
            // but replaced/ambiguous device is a hard failure.
            Err(status) if status.code() == tonic::Code::Unavailable => Ok(()),
            Err(status) => Err(status),
        }
    }

    fn verify_mount(&self, volume: &Volume, mount: &Mount, path: &Path) -> Result<(), Status> {
        match &volume.spec.mode {
            Mode::Filesystem(fs_type)
                if mount.major == volume.device.major
                    && mount.minor == volume.device.minor
                    && &mount.fs_type == fs_type
                    && mount.root == Path::new("/") =>
            {
                Ok(())
            }
            Mode::Block
                if self.host.block_identity(path)?
                    == (volume.device.major, volume.device.minor) =>
            {
                Ok(())
            }
            _ => Err(Status::failed_precondition(
                "target is mounted from another device or filesystem; refusing to touch it",
            )),
        }
    }

    fn verify_mount_flags(mount: &Mount, desired: &[String]) -> Result<(), Status> {
        for flag in ["nosuid", "nodev", "noexec"] {
            if desired.iter().any(|wanted| wanted == flag) && !mount.options.contains(flag) {
                return Err(Status::failed_precondition(format!(
                    "mount is missing required {flag} flag"
                )));
            }
        }
        let readonly = desired.iter().any(|flag| flag == "ro");
        if mount.options.contains("ro") != readonly {
            return Err(Status::failed_precondition(
                "mount has different read-only mode",
            ));
        }
        Ok(())
    }

    fn create_directory(&self, path: &Path) -> Result<(), Status> {
        no_symlinks(path)?;
        std::fs::create_dir_all(path)
            .map_err(|e| Status::internal(format!("create volume directory: {e}")))?;
        no_symlinks(path)
    }

    async fn stage_mount(&self, volume: &Volume) -> Result<(), Status> {
        let mounts = self.host.mounts().await?;
        if let Some(mount) = unique_mount(&mounts, &volume.staging_path)? {
            if matches!(volume.spec.mode, Mode::Block) {
                return Err(Status::failed_precondition(
                    "block staging directory unexpectedly mounted",
                ));
            }
            self.verify_mount(volume, mount, &volume.staging_path)?;
            Self::verify_mount_flags(mount, &volume.spec.flags)?;
            return Ok(());
        }
        if mounts
            .iter()
            .any(|mount| mount.target.starts_with(&volume.staging_path))
        {
            return Err(Status::failed_precondition(
                "staging path contains other mounts",
            ));
        }
        self.create_directory(&volume.staging_path)?;
        if let Mode::Filesystem(fs_type) = &volume.spec.mode {
            self.host
                .mount_fs(
                    &volume.device,
                    &volume.staging_path,
                    fs_type,
                    &volume.spec.flags,
                )
                .await?;
            let mounts = self.host.mounts().await?;
            let mount = unique_mount(&mounts, &volume.staging_path)?
                .ok_or_else(|| Status::internal("mount command succeeded without staging mount"))?;
            self.verify_mount(volume, mount, &volume.staging_path)?;
            Self::verify_mount_flags(mount, &volume.spec.flags)?;
        }
        Ok(())
    }

    async fn checked_volume_path(&self, volume: &Volume, path: &Path) -> Result<(), Status> {
        if path != volume.staging_path && !volume.published.contains_key(path) {
            return Err(Status::not_found("path is not registered for this volume"));
        }
        self.current_device(volume).await?;
        let mounts = self.host.mounts().await?;
        if matches!(volume.spec.mode, Mode::Block) && path == volume.staging_path {
            if mounts.iter().any(|mount| mount.target.starts_with(path)) {
                return Err(Status::failed_precondition(
                    "raw block staging directory unexpectedly contains mounts",
                ));
            }
            return Ok(());
        }
        let mount = unique_mount(&mounts, path)?
            .ok_or_else(|| Status::not_found("volume path is not mounted"))?;
        self.verify_mount(volume, mount, path)
    }
}

#[tonic::async_trait]
impl csi::node_server::Node for NodeService {
    async fn node_stage_volume(
        &self,
        request: Request<csi::NodeStageVolumeRequest>,
    ) -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let path = self.path(&request.staging_target_path)?;
        let serial = request
            .publish_context
            .get("device_serial")
            .ok_or_else(|| Status::invalid_argument("publish_context.device_serial is required"))?;
        validate_token(serial, "device_serial", 256)?;
        let spec = spec(request.volume_capability.as_ref(), &request.publish_context)?;
        let mut registry = self.registry.lock().await;
        Self::require_unclaimed_path(&registry, &path, &request.volume_id, true)?;
        let device = self.host.discover(serial).await?;
        if registry.volumes.iter().any(|(id, volume)| {
            id != &request.volume_id && (volume.serial == *serial || volume.device == device)
        }) {
            return Err(Status::already_exists(
                "device is already staged as a different volume",
            ));
        }
        if let Mode::Filesystem(fs_type) = &spec.mode
            && self.host.filesystem(&device).await?.as_ref() != Some(fs_type)
        {
            return Err(Status::failed_precondition(
                "device lacks the expected preformatted filesystem; formatting is forbidden, including for adopted volumes",
            ));
        }
        let volume = if let Some(existing) = registry.volumes.get(&request.volume_id) {
            if existing.serial != *serial
                || existing.device != device
                || existing.staging_path != path
                || existing.spec != spec
            {
                return Err(Status::already_exists(
                    "volume is staged with different parameters",
                ));
            }
            existing.clone()
        } else {
            // Do not take ownership of mounts created outside this state registry.
            let mounts = self.host.mounts().await?;
            if mounts.iter().any(|mount| {
                mount.target.starts_with(&path)
                    || (mount.major == device.major && mount.minor == device.minor)
            }) {
                return Err(Status::failed_precondition(
                    "unregistered mount exists for this device or staging path",
                ));
            }
            let volume = Volume {
                serial: serial.clone(),
                device,
                staging_path: path,
                spec,
                published: BTreeMap::new(),
            };
            let mut next = registry.clone();
            next.volumes.insert(request.volume_id, volume.clone());
            self.update(&mut registry, next)?;
            volume
        };
        self.stage_mount(&volume).await?;
        Ok(Response::new(csi::NodeStageVolumeResponse {}))
    }

    async fn node_publish_volume(
        &self,
        request: Request<csi::NodePublishVolumeRequest>,
    ) -> Result<Response<csi::NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.target_path)?;
        let staging = self.path(&request.staging_target_path)?;
        let desired = spec(request.volume_capability.as_ref(), &request.publish_context)?;
        let mut registry = self.registry.lock().await;
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::failed_precondition("volume must be staged first"))?
            .clone();
        if staging != volume.staging_path
            || desired != volume.spec
            || request.publish_context.get("device_serial") != Some(&volume.serial)
        {
            return Err(Status::invalid_argument(
                "publish parameters differ from staged volume",
            ));
        }
        if !request.readonly && volume.spec.flags.iter().any(|flag| flag == "ro") {
            return Err(Status::failed_precondition(
                "cannot publish a read-only stage as writable",
            ));
        }
        Self::require_unclaimed_path(&registry, &target, &request.volume_id, false)?;
        if volume.spec.access_mode == 6 && volume.published.keys().any(|path| path != &target) {
            return Err(Status::failed_precondition(
                "SINGLE_NODE_SINGLE_WRITER already has a publication",
            ));
        }
        self.current_device(&volume).await?;
        if matches!(volume.spec.mode, Mode::Filesystem(_)) {
            self.checked_volume_path(&volume, &staging).await?;
        }
        let readonly = request.readonly;
        let publish_flags = flags(
            &volume.spec.flags,
            matches!(volume.spec.mode, Mode::Block),
            readonly,
        )?;
        let mounts = self.host.mounts().await?;
        let existing_mount = unique_mount(&mounts, &target)?;
        if let Some(previous) = volume.published.get(&target) {
            if *previous != readonly {
                return Err(Status::already_exists(
                    "target was published with different read-only mode",
                ));
            }
        } else {
            if existing_mount.is_some()
                || mounts.iter().any(|mount| mount.target.starts_with(&target))
            {
                return Err(Status::failed_precondition(
                    "unregistered mount exists at publish target",
                ));
            }
            let mut next = registry.clone();
            next.volumes
                .get_mut(&request.volume_id)
                .expect("registered volume")
                .published
                .insert(target.clone(), readonly);
            self.update(&mut registry, next)?;
        }
        if let Some(mount) = existing_mount {
            self.verify_mount(&volume, mount, &target)?;
            // Complete a bind whose remount was interrupted after the bind syscall.
            self.host.remount_bind(&target, &publish_flags).await?;
        } else {
            let source = match volume.spec.mode {
                Mode::Filesystem(_) => {
                    self.create_directory(&target)?;
                    staging
                }
                Mode::Block => {
                    self.create_directory(
                        target
                            .parent()
                            .ok_or_else(|| Status::invalid_argument("target has no parent"))?,
                    )?;
                    no_symlinks(&target)?;
                    match std::fs::symlink_metadata(&target) {
                        Ok(metadata) if !metadata.is_file() || metadata.len() != 0 => {
                            return Err(Status::failed_precondition(
                                "block target must be an empty regular file",
                            ));
                        }
                        Ok(_) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .mode(0o600)
                                .custom_flags(nix::libc::O_NOFOLLOW)
                                .open(&target)
                                .map_err(|e| {
                                    Status::internal(format!("create block target: {e}"))
                                })?;
                        }
                        Err(error) => {
                            return Err(Status::internal(format!("inspect block target: {error}")));
                        }
                    }
                    volume.device.path.clone()
                }
            };
            self.host.bind(&source, &target, &publish_flags).await?;
        }
        let mounts = self.host.mounts().await?;
        let mount = unique_mount(&mounts, &target)?
            .ok_or_else(|| Status::internal("publish command succeeded without mount"))?;
        self.verify_mount(&volume, mount, &target)?;
        Self::verify_mount_flags(mount, &publish_flags)?;
        Ok(Response::new(csi::NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<csi::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<csi::NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.target_path)?;
        let mut registry = self.registry.lock().await;
        let mounts = self.host.mounts().await?;
        let mounted = unique_mount(&mounts, &target)?;
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .filter(|v| v.published.contains_key(&target));
        if let Some(mount) = mounted {
            let volume = volume.ok_or_else(|| {
                Status::failed_precondition("refusing to unmount an unregistered target")
            })?;
            self.device_for_cleanup(volume).await?;
            self.verify_mount(volume, mount, &target)?;
            if mounts
                .iter()
                .any(|m| m.target != target && m.target.starts_with(&target))
            {
                return Err(Status::failed_precondition(
                    "publish target contains nested mounts",
                ));
            }
            self.host.unmount(&target).await?;
            if unique_mount(&self.host.mounts().await?, &target)?.is_some() {
                return Err(Status::internal("target remains mounted"));
            }
        }
        if volume.is_some() {
            let mut next = registry.clone();
            next.volumes
                .get_mut(&request.volume_id)
                .expect("registered volume")
                .published
                .remove(&target);
            self.update(&mut registry, next)?;
        }
        Ok(Response::new(csi::NodeUnpublishVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<csi::NodeUnstageVolumeRequest>,
    ) -> Result<Response<csi::NodeUnstageVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.staging_target_path)?;
        let mut registry = self.registry.lock().await;
        let mounts = self.host.mounts().await?;
        let mounted = unique_mount(&mounts, &target)?;
        if let Some(volume) = registry.volumes.get(&request.volume_id) {
            if volume.staging_path != target {
                return Err(Status::invalid_argument(
                    "staging path differs from registered volume",
                ));
            }
            if !volume.published.is_empty() {
                return Err(Status::failed_precondition(
                    "volume still has published targets",
                ));
            }
            if mounts
                .iter()
                .any(|m| m.target != target && m.target.starts_with(&target))
            {
                return Err(Status::failed_precondition(
                    "staging target contains nested mounts",
                ));
            }
            if let Some(mount) = mounted {
                self.device_for_cleanup(volume).await?;
                self.verify_mount(volume, mount, &target)?;
                self.host.unmount(&target).await?;
                if unique_mount(&self.host.mounts().await?, &target)?.is_some() {
                    return Err(Status::internal("staging path remains mounted"));
                }
            }
            let mut next = registry.clone();
            next.volumes.remove(&request.volume_id);
            self.update(&mut registry, next)?;
        } else if mounted.is_some() || mounts.iter().any(|m| m.target.starts_with(&target)) {
            return Err(Status::failed_precondition(
                "refusing to unstage an unregistered mount",
            ));
        }
        Ok(Response::new(csi::NodeUnstageVolumeResponse {}))
    }

    async fn node_get_info(
        &self,
        _request: Request<csi::NodeGetInfoRequest>,
    ) -> Result<Response<csi::NodeGetInfoResponse>, Status> {
        Ok(Response::new(csi::NodeGetInfoResponse {
            node_id: self.config.node_id.clone(),
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<csi::NodeGetCapabilitiesRequest>,
    ) -> Result<Response<csi::NodeGetCapabilitiesResponse>, Status> {
        use csi::node_service_capability::{Rpc, Type as Capability, rpc::Type};
        let capabilities = [
            Type::StageUnstageVolume,
            Type::GetVolumeStats,
            Type::ExpandVolume,
        ]
        .into_iter()
        .map(|r#type| csi::NodeServiceCapability {
            r#type: Some(Capability::Rpc(Rpc {
                r#type: r#type as i32,
            })),
        })
        .collect();
        Ok(Response::new(csi::NodeGetCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn node_get_volume_stats(
        &self,
        request: Request<csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<csi::NodeGetVolumeStatsResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.volume_path)?;
        let registry = self.registry.lock().await;
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::not_found("volume is not staged"))?;
        if !request.staging_target_path.is_empty()
            && self.path(&request.staging_target_path)? != volume.staging_path
        {
            return Err(Status::invalid_argument(
                "staging path differs from registered volume",
            ));
        }
        self.checked_volume_path(volume, &target).await?;
        let usage = match volume.spec.mode {
            Mode::Block => vec![csi::VolumeUsage {
                available: 0,
                total: self.host.capacity(&volume.device).await?,
                used: 0,
                unit: csi::volume_usage::Unit::Bytes as i32,
            }],
            Mode::Filesystem(_) => {
                let stat = self.host.usage(&target)?;
                vec![
                    csi::VolumeUsage {
                        available: stat.available,
                        total: stat.total,
                        used: stat.used,
                        unit: csi::volume_usage::Unit::Bytes as i32,
                    },
                    csi::VolumeUsage {
                        available: stat.inodes_available,
                        total: stat.inodes_total,
                        used: stat.inodes_used,
                        unit: csi::volume_usage::Unit::Inodes as i32,
                    },
                ]
            }
        };
        Ok(Response::new(csi::NodeGetVolumeStatsResponse {
            usage,
            volume_condition: None,
        }))
    }

    async fn node_expand_volume(
        &self,
        request: Request<csi::NodeExpandVolumeRequest>,
    ) -> Result<Response<csi::NodeExpandVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.volume_path)?;
        let range = match request.capacity_range {
            Some(range) if range.required_bytes == 0 && range.limit_bytes == 0 => {
                return Err(Status::invalid_argument(
                    "a supplied capacity_range must specify required_bytes or limit_bytes",
                ));
            }
            Some(range) => range,
            // CSI permits NodeExpand to inspect the device when the optional
            // capacity_range is absent. An explicitly empty range is invalid.
            None => csi::CapacityRange::default(),
        };
        if range.required_bytes < 0
            || range.limit_bytes < 0
            || (range.limit_bytes != 0 && range.required_bytes > range.limit_bytes)
        {
            return Err(Status::invalid_argument("invalid capacity range"));
        }
        let registry = self.registry.lock().await;
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::not_found("volume is not staged"))?;
        self.checked_volume_path(volume, &target).await?;
        if !request.staging_target_path.is_empty()
            && self.path(&request.staging_target_path)? != volume.staging_path
        {
            return Err(Status::invalid_argument("staging path differs from volume"));
        }
        if volume.spec.flags.iter().any(|f| f == "ro")
            || volume.published.get(&target) == Some(&true)
        {
            return Err(Status::failed_precondition(
                "cannot expand a read-only volume",
            ));
        }
        if let Some(capability) = request.volume_capability {
            let context = HashMap::from([
                ("format_policy".into(), "preformatted".into()),
                (
                    "volume_mode".into(),
                    match volume.spec.mode {
                        Mode::Block => "block",
                        _ => "filesystem",
                    }
                    .into(),
                ),
            ]);
            let mut context = context;
            if let Mode::Filesystem(fs_type) = &volume.spec.mode {
                context.insert("fs_type".into(), fs_type.clone());
            }
            if spec(Some(&capability), &context)? != volume.spec {
                return Err(Status::invalid_argument(
                    "expand capability differs from staged volume",
                ));
            }
        }
        let mut bytes = self.host.capacity(&volume.device).await?;
        if bytes < range.required_bytes {
            self.host.rescan(&volume.device, &volume.serial).await?;
            self.current_device(volume).await?;
            bytes = self.host.capacity(&volume.device).await?;
        }
        if bytes < range.required_bytes {
            return Err(Status::failed_precondition(
                "block device expansion is not yet visible on this node",
            ));
        }
        if range.limit_bytes != 0 && bytes > range.limit_bytes {
            return Err(Status::out_of_range(
                "device capacity exceeds requested limit",
            ));
        }
        if let Mode::Filesystem(fs_type) = &volume.spec.mode {
            self.host.grow(&volume.device, &target, fs_type).await?;
        }
        Ok(Response::new(csi::NodeExpandVolumeResponse {
            capacity_bytes: bytes,
        }))
    }
}
