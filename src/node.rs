//! Linux CSI node service. Axo supplies preformatted devices; this module never
//! formats a disk and never talks to the cloud or to Ceph.
mod host;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    future::Future,
    io::Write,
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
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

impl Spec {
    fn same_identity_and_security(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.access_mode == other.access_mode
            && ["ro", "nosuid", "nodev", "noexec"].iter().all(|flag| {
                self.flags.iter().any(|value| value == flag)
                    == other.flags.iter().any(|value| value == flag)
            })
    }

    fn replace_security_flags(&mut self, requested: &Self) {
        let security = ["ro", "rw", "nosuid", "nodev", "noexec"];
        self.flags.retain(|flag| !security.contains(&flag.as_str()));
        self.flags.extend(
            requested
                .flags
                .iter()
                .filter(|flag| security.contains(&flag.as_str()))
                .cloned(),
        );
        self.flags.sort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone)]
pub struct NodeService {
    config: NodeConfig,
    registry: Arc<Mutex<Registry>>,
    operations: Arc<StdMutex<BTreeSet<String>>>,
    idle: Arc<Notify>,
    host: Arc<dyn Host>,
    _process_lock: Arc<File>,
}

struct OperationGuard {
    id: String,
    operations: Arc<StdMutex<BTreeSet<String>>>,
    idle: Arc<Notify>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.operations
            .lock()
            .expect("operation registry")
            .remove(&self.id);
        self.idle.notify_waiters();
    }
}

async fn blocking<T: Send + 'static>(
    action: impl FnOnce() -> Result<T, Status> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(action)
        .await
        .map_err(|_| Status::aborted("node filesystem operation did not complete"))?
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

fn validate_serial(value: &str) -> Result<(), Status> {
    if value.is_empty()
        || value.len() > 36
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(Status::invalid_argument(
            "device_serial must match [A-Za-z0-9._-]{1,36}",
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
    fn verify_incarnation(&self, context: &HashMap<String, String>) -> Result<(), Status> {
        let node_id = context
            .get("node_id")
            .ok_or_else(|| Status::invalid_argument("publish_context.node_id is required"))?;
        validate_token(node_id, "publish_context.node_id", 256)?;
        if node_id != &self.config.node_id {
            return Err(Status::failed_precondition(
                "publish_context must identify this VM incarnation; detach and reattach the volume",
            ));
        }
        Ok(())
    }

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
        // Children inherit this open file description. Closing the last copy,
        // rather than explicit LOCK_UN, releases ownership after a crash.
        if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) }
            != 0
        {
            return Err(NodeError::Locked);
        }
        let lock = Arc::new(file);
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
            validate_serial(&volume.serial)
                .map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
            for path in std::iter::once(&volume.staging_path).chain(volume.published.keys()) {
                target_path(&path.to_string_lossy(), &config.kubelet_root)
                    .map_err(|e| NodeError::Configuration(e.message().to_owned()))?;
            }
        }
        let host = Arc::new(LinuxHost {
            device_root: config.device_root.clone(),
            sys_block_dir: config.sys_block_dir.clone(),
            process_lock: lock.clone(),
        });
        let service = Self {
            config,
            registry: Arc::new(Mutex::new(registry.clone())),
            operations: Arc::new(StdMutex::new(BTreeSet::new())),
            idle: Arc::new(Notify::new()),
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

    async fn update(&self, current: &mut Registry, next: Registry) -> Result<(), Status> {
        let previous = current.clone();
        let mut actual = self.registry.clone().lock_owned().await;
        let service = self.clone();
        let merged = blocking(move || {
            let mut merged = actual.clone();
            let ids: BTreeSet<_> = previous.volumes.keys().chain(next.volumes.keys()).collect();
            for id in ids {
                if previous.volumes.get(id) == next.volumes.get(id) {
                    continue;
                }
                if actual.volumes.get(id) != previous.volumes.get(id) {
                    return Err(Status::aborted(
                        "volume registration changed during operation",
                    ));
                }
                match next.volumes.get(id) {
                    Some(volume) => {
                        merged.volumes.insert(id.clone(), volume.clone());
                    }
                    None => {
                        merged.volumes.remove(id);
                    }
                }
            }
            for (id, volume) in &merged.volumes {
                Self::require_unclaimed_path(&merged, &volume.staging_path, id, true)?;
                for path in volume.published.keys() {
                    Self::require_unclaimed_path(&merged, path, id, false)?;
                }
                if merged.volumes.iter().any(|(other_id, other)| {
                    other_id != id
                        && (other.serial == volume.serial
                            || (other.device.major, other.device.minor)
                                == (volume.device.major, volume.device.minor))
                }) {
                    return Err(Status::aborted(
                        "device registration is reserved by another volume",
                    ));
                }
            }
            service
                .persist(&merged)
                .map_err(|e| Status::aborted(format!("persist node state: {e}")))?;
            *actual = merged.clone();
            Ok(merged)
        })
        .await?;
        *current = merged;
        Ok(())
    }

    fn claim(&self, id: &str) -> Result<OperationGuard, Status> {
        if !self
            .operations
            .lock()
            .expect("operation registry")
            .insert(id.to_owned())
        {
            return Err(Status::aborted(
                "another operation is still running for this volume",
            ));
        }
        Ok(OperationGuard {
            id: id.to_owned(),
            operations: self.operations.clone(),
            idle: self.idle.clone(),
        })
    }

    async fn run<
        T: Send + 'static,
        F: Future<Output = Result<Response<T>, Status>> + Send + 'static,
    >(
        &self,
        name: &'static str,
        id: String,
        operation: impl FnOnce(Self) -> F + Send + 'static,
    ) -> Result<Response<T>, Status> {
        validate_volume_id(&id)?;
        let guard = self.claim(&id)?;
        let service = self.clone();
        // Dropping the RPC only detaches this task. Its lock covers the actual
        // command termination and durable state update, including on timeout.
        tokio::spawn(async move {
            let _guard = guard;
            let started = Instant::now();
            let result = operation(service).await;
            if let Err(error) = &result {
                tracing::warn!(operation = name, code = ?error.code(), elapsed_ms = started.elapsed().as_millis(), "node operation failed");
            }
            result
        })
        .await
        .map_err(|_| Status::aborted("node operation did not complete"))?
    }

    pub async fn drain(&self) {
        loop {
            let notified = self.idle.notified();
            if self
                .operations
                .lock()
                .expect("operation registry")
                .is_empty()
            {
                return;
            }
            notified.await;
        }
    }

    fn lexical_path(&self, value: &str) -> Result<PathBuf, Status> {
        let path = target_path(value, &self.config.kubelet_root)?;
        if path.starts_with(&self.config.state_dir) || self.config.state_dir.starts_with(&path) {
            return Err(Status::invalid_argument(
                "volume target overlaps the driver state directory",
            ));
        }
        Ok(path)
    }

    async fn path(&self, value: &str) -> Result<PathBuf, Status> {
        let path = self.lexical_path(value)?;
        blocking(move || {
            no_symlinks(&path)?;
            Ok(path)
        })
        .await
    }

    async fn cleanup_path(&self, path: &Path, mounted: bool) -> Result<(), Status> {
        // A mounted filesystem may return EIO from getattr during shutdown.
        // Its identity is verified through mountinfo, without entering it.
        let inspect = if mounted {
            path.parent().unwrap_or(path)
        } else {
            path
        }
        .to_owned();
        blocking(move || no_symlinks(&inspect)).await
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
                "device identity changed since staging; restage after all mounts are gone",
            ));
        }
        Ok(device)
    }

    async fn require_no_device_mounts(
        &self,
        mounts: &[Mount],
        devices: &[&Device],
        paths: &[&Path],
    ) -> Result<(), Status> {
        let mut device_filesystems = BTreeSet::new();
        for device in devices {
            if let Some(identity) = self.host.device_filesystem(device).await? {
                device_filesystems.insert(identity);
            }
        }
        for mount in mounts {
            // For a raw block bind, mountinfo describes devtmpfs, while the
            // target inode's st_rdev identifies the actual volume device.
            let raw_device = if matches!(mount.fs_type.as_str(), "devtmpfs" | "tmpfs")
                || device_filesystems.contains(&(mount.major, mount.minor))
            {
                self.host.block_identity(&mount.target).await?
            } else {
                None
            };
            if paths.iter().any(|path| mount.target.starts_with(path))
                || devices.iter().any(|device| {
                    (mount.major, mount.minor) == (device.major, device.minor)
                        || raw_device == Some((device.major, device.minor))
                })
            {
                return Err(Status::failed_precondition(
                    "mounts still reference the device or its registered paths; refusing ownership change",
                ));
            }
        }
        Ok(())
    }

    async fn reconcile_conflicting_registrations(
        &self,
        next: &mut Registry,
        owner: &str,
        device: &Device,
    ) -> Result<Vec<OperationGuard>, Status> {
        let mut guards = Vec::new();
        let mut resolved = BTreeSet::new();
        let mut pending = vec![device.clone()];
        while let Some(candidate) = pending.pop() {
            let conflicts: Vec<_> = next
                .volumes
                .iter()
                .filter(|(id, volume)| {
                    id.as_str() != owner
                        && !resolved.contains(id.as_str())
                        && (volume.device.major, volume.device.minor)
                            == (candidate.major, candidate.minor)
                })
                .map(|(id, volume)| (id.clone(), volume.clone()))
                .collect();
            for (id, mut volume) in conflicts {
                guards.push(self.claim(&id)?);
                resolved.insert(id.clone());
                let current = match self.host.discover(&volume.serial).await {
                    Ok(current) => Some(current),
                    Err(status) if status.code() == tonic::Code::Unavailable => None,
                    Err(status) => return Err(status),
                };
                if current.as_ref().is_some_and(|current| {
                    (current.major, current.minor) == (candidate.major, candidate.minor)
                }) {
                    return Err(Status::already_exists("device is staged as another volume"));
                }
                let paths: Vec<&Path> = std::iter::once(volume.staging_path.as_path())
                    .chain(volume.published.keys().map(PathBuf::as_path))
                    .collect();
                let mut devices = vec![&volume.device];
                if let Some(current) = &current {
                    devices.push(current);
                }
                self.require_no_device_mounts(&self.host.mounts().await?, &devices, &paths)
                    .await?;
                if let Some(current) = current {
                    self.host.ensure_released(&current).await?;
                    pending.push(current.clone());
                    volume.device = current;
                    next.volumes.insert(id, volume);
                } else {
                    next.volumes.remove(&id);
                }
            }
        }
        Ok(guards)
    }

    async fn device_for_cleanup(&self, volume: &Volume) -> Result<(), Status> {
        match self.host.discover(&volume.serial).await {
            Ok(_) => Ok(()),
            // Hot-unplug can remove sysfs before kubelet requests cleanup. The
            // caller must still match the recorded mount identity. A new device
            // for this serial must not prevent removing the old owned mounts.
            Err(status) if status.code() == tonic::Code::Unavailable => Ok(()),
            Err(status) => Err(status),
        }
    }

    async fn verify_mount(
        &self,
        volume: &Volume,
        mount: &Mount,
        path: &Path,
    ) -> Result<(), Status> {
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
                if self.host.block_identity(path).await?
                    == Some((volume.device.major, volume.device.minor)) =>
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

    async fn create_directory(&self, path: &Path) -> Result<(), Status> {
        let path = path.to_owned();
        blocking(move || {
            no_symlinks(&path)?;
            std::fs::create_dir_all(&path)
                .map_err(|e| Status::internal(format!("create volume directory: {e}")))?;
            no_symlinks(&path)
        })
        .await
    }

    async fn remove_publish_target(&self, volume: &Volume, target: &Path) -> Result<(), Status> {
        let volume = volume.clone();
        let target = target.to_owned();
        blocking(move || {
            let target = target.as_path();
            // Only called for a persisted publication after verifying that no mounts
            // remain. Never follow symlinks or recursively delete content.
            no_symlinks(target)?;
            let metadata = match std::fs::symlink_metadata(target) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(Status::internal(format!("inspect publish target: {error}")));
                }
            };
            if let Some(metadata) = metadata {
                let removed = match volume.spec.mode {
                    Mode::Filesystem(_) if metadata.is_dir() => std::fs::remove_dir(target),
                    Mode::Block if metadata.is_file() && metadata.len() == 0 => {
                        std::fs::remove_file(target)
                    }
                    _ => {
                        return Err(Status::failed_precondition(
                            "publish target is not an empty owned placeholder",
                        ));
                    }
                };
                removed.map_err(|_| {
                    Status::failed_precondition(
                        "cannot remove publish target; it must be empty and unmounted",
                    )
                })?;
            }
            // Also sync on retry after a successful unlink but failed directory
            // fsync. Only then may the durable publication intent be forgotten.
            if let Some(parent) = target.parent() {
                match File::open(parent) {
                    Ok(directory) => directory.sync_all().map_err(|error| {
                        Status::internal(format!("persist target removal: {error}"))
                    })?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => {
                        return Err(Status::internal(format!("persist target removal: {error}")));
                    }
                }
            }
            Ok(())
        })
        .await
    }

    async fn stage_mount(&self, volume: &Volume) -> Result<(), Status> {
        let mounts = self.host.mounts().await?;
        if let Some(mount) = unique_mount(&mounts, &volume.staging_path)? {
            if matches!(volume.spec.mode, Mode::Block) {
                return Err(Status::failed_precondition(
                    "block staging directory unexpectedly mounted",
                ));
            }
            self.verify_mount(volume, mount, &volume.staging_path)
                .await?;
            Self::verify_mount_flags(mount, &volume.spec.flags)?;
            return self.grow_staged_filesystem(volume).await;
        }
        if mounts
            .iter()
            .any(|mount| mount.target.starts_with(&volume.staging_path))
        {
            return Err(Status::failed_precondition(
                "staging path contains other mounts",
            ));
        }
        self.create_directory(&volume.staging_path).await?;
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
            self.verify_mount(volume, mount, &volume.staging_path)
                .await?;
            Self::verify_mount_flags(mount, &volume.spec.flags)?;
        }
        self.grow_staged_filesystem(volume).await
    }

    async fn grow_staged_filesystem(&self, volume: &Volume) -> Result<(), Status> {
        if let Mode::Filesystem(fs_type) = &volume.spec.mode
            && !volume.spec.flags.iter().any(|flag| flag == "ro")
        {
            self.current_device(volume).await?;
            // Clone/restore can expose a larger device with the source's smaller
            // filesystem. Both grow tools are idempotent when already full-sized.
            // Run on retries too, so a completed mount is not mistaken for a
            // completed stage after an interrupted or failed filesystem growth.
            self.host
                .grow(&volume.device, &volume.staging_path, fs_type)
                .await?;
        }
        Ok(())
    }

    async fn checked_volume_path(&self, volume: &Volume, path: &Path) -> Result<(), Status> {
        let block_alias = if matches!(volume.spec.mode, Mode::Block) {
            let base = self
                .config
                .kubelet_root
                .join("plugins/kubernetes.io/csi/volumeDevices");
            path.strip_prefix(&base).ok().is_some_and(|relative| {
                let parts: Vec<_> = relative.components().collect();
                parts.len() == 3
                    && parts[1].as_os_str() == "dev"
                    && volume.published.contains_key(
                        &base
                            .join("publish")
                            .join(parts[0].as_os_str())
                            .join(parts[2].as_os_str()),
                    )
            })
        } else {
            false
        };
        if path != volume.staging_path && !volume.published.contains_key(path) && !block_alias {
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
        self.verify_mount(volume, mount, path).await
    }
}

impl NodeService {
    async fn stage_volume(
        &self,
        request: Request<csi::NodeStageVolumeRequest>,
    ) -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        let request = request.into_inner();
        self.verify_incarnation(&request.publish_context)?;
        let path = self.path(&request.staging_target_path).await?;
        let serial = request
            .publish_context
            .get("device_serial")
            .ok_or_else(|| Status::invalid_argument("publish_context.device_serial is required"))?;
        validate_serial(serial)?;
        let spec = spec(request.volume_capability.as_ref(), &request.publish_context)?;
        let mut registry = self.registry.lock().await.clone();
        Self::require_unclaimed_path(&registry, &path, &request.volume_id, true)?;
        let device = self.host.discover(serial).await?;
        if registry
            .volumes
            .iter()
            .any(|(id, volume)| id != &request.volume_id && volume.serial == *serial)
        {
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
        let previous = registry.volumes.get(&request.volume_id).cloned();
        if let Some(existing) = &previous
            && (existing.serial != *serial
                || existing.staging_path != path
                || existing.spec.mode != spec.mode
                || existing.spec.access_mode != spec.access_mode)
        {
            return Err(Status::already_exists(
                "volume is staged with different parameters",
            ));
        }
        let mut next = registry.clone();
        let _conflicts = self
            .reconcile_conflicting_registrations(&mut next, &request.volume_id, &device)
            .await?;
        let volume = if let Some(mut volume) = previous.clone() {
            if volume.device != device || !volume.spec.same_identity_and_security(&spec) {
                let mounts = self.host.mounts().await?;
                let paths: Vec<&Path> = std::iter::once(volume.staging_path.as_path())
                    .chain(volume.published.keys().map(PathBuf::as_path))
                    .collect();
                self.require_no_device_mounts(&mounts, &[&volume.device, &device], &paths)
                    .await?;
                self.host.ensure_released(&device).await?;
                volume.device = device;
                volume.spec.replace_security_flags(&spec);
            }
            // Harmless PV mountOptions edits do not alter the options actually
            // applied to an existing stage, including after a process restart.
            // Those edits take effect after a complete unstage and fresh stage.
            volume
        } else {
            self.require_no_device_mounts(&self.host.mounts().await?, &[&device], &[&path])
                .await?;
            self.host.ensure_released(&device).await?;
            Volume {
                serial: serial.clone(),
                device,
                staging_path: path,
                spec,
                published: BTreeMap::new(),
            }
        };
        next.volumes
            .insert(request.volume_id.clone(), volume.clone());
        self.update(&mut registry, next).await?;
        if let Err(error) = self.stage_mount(&volume).await {
            // A final CSI error promises that no uncertain effect needs an
            // inverse operation. Only make that promise after proving rollback.
            let rollback = async {
                if previous.is_some() {
                    return Err(Status::aborted("staging recovery is incomplete"));
                }
                self.require_no_device_mounts(
                    &self.host.mounts().await?,
                    &[&volume.device],
                    &[&volume.staging_path],
                )
                .await?;
                self.host.ensure_released(&volume.device).await?;
                let mut next = registry.clone();
                next.volumes.remove(&request.volume_id);
                self.update(&mut registry, next).await
            }
            .await;
            return Err(if rollback.is_ok() {
                error
            } else {
                Status::aborted("staging effect requires retry or cleanup")
            });
        }
        Ok(Response::new(csi::NodeStageVolumeResponse {}))
    }

    async fn publish_volume(
        &self,
        request: Request<csi::NodePublishVolumeRequest>,
    ) -> Result<Response<csi::NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        self.verify_incarnation(&request.publish_context)?;
        validate_volume_id(&request.volume_id)?;
        let target = self.path(&request.target_path).await?;
        let staging = self.path(&request.staging_target_path).await?;
        let desired = spec(request.volume_capability.as_ref(), &request.publish_context)?;
        let mut registry = self.registry.lock().await.clone();
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::failed_precondition("volume must be staged first"))?
            .clone();
        if staging != volume.staging_path
            || !desired.same_identity_and_security(&volume.spec)
            || request.publish_context.get("device_serial") != Some(&volume.serial)
        {
            return Err(Status::invalid_argument(
                "publish parameters differ from staged volume",
            ));
        }
        Self::require_unclaimed_path(&registry, &target, &request.volume_id, false)?;
        self.current_device(&volume).await?;
        if matches!(volume.spec.mode, Mode::Filesystem(_)) {
            self.checked_volume_path(&volume, &staging).await?;
        }
        let readonly = request.readonly || volume.spec.flags.iter().any(|flag| flag == "ro");
        if matches!(volume.spec.mode, Mode::Block) && readonly {
            // A read-only bind mount does not prevent writes to a device inode.
            // Never set device-wide RO here: another pod may already use it RW.
            if request.publish_context.get("readonly").map(String::as_str) != Some("true")
                || !self.host.readonly(&volume.device).await?
            {
                return Err(Status::failed_precondition(
                    "read-only block publication requires a read-only backend attachment and verified Linux block protection",
                ));
            }
            self.current_device(&volume).await?;
        }
        let publish_flags = flags(
            &volume.spec.flags,
            matches!(volume.spec.mode, Mode::Block),
            readonly,
        )?;
        let mounts = self.host.mounts().await?;
        let existing_mount = unique_mount(&mounts, &target)?;
        let registered = volume.published.contains_key(&target);
        let target_has_mounts = mounts.iter().any(|mount| mount.target.starts_with(&target));
        if let Some(previous) = volume.published.get(&target) {
            if *previous != readonly && target_has_mounts {
                return Err(Status::already_exists(
                    "target was published with different read-only mode",
                ));
            }
        } else if target_has_mounts {
            return Err(Status::failed_precondition(
                "unregistered mount exists at publish target",
            ));
        }
        if volume.published.get(&target) != Some(&readonly) {
            // A persisted intent can outlive its mount (for example on reboot).
            // Only recompute its effective RO mode after proving no target or
            // descendant mount remains; persist it before attempting the bind.
            let mut next = registry.clone();
            next.volumes
                .get_mut(&request.volume_id)
                .expect("registered volume")
                .published
                .insert(target.clone(), readonly);
            self.update(&mut registry, next).await?;
        }
        let mut prepared = false;
        let effect = async {
            let source = if existing_mount.is_none() {
                match volume.spec.mode {
                    Mode::Filesystem(_) => {
                        self.create_directory(&target).await?;
                        let directory = target.clone();
                        blocking(move || {
                            if std::fs::read_dir(directory)
                                .map_err(|e| {
                                    Status::internal(format!("inspect publish directory: {e}"))
                                })?
                                .next()
                                .is_some()
                            {
                                return Err(Status::failed_precondition(
                                    "filesystem target must be an empty directory",
                                ));
                            }
                            Ok(())
                        })
                        .await?;
                        staging
                    }
                    Mode::Block => {
                        self.create_directory(
                            target
                                .parent()
                                .ok_or_else(|| Status::invalid_argument("target has no parent"))?,
                        )
                        .await?;
                        let file_path = target.clone();
                        blocking(move || {
                            no_symlinks(&file_path)?;
                            match std::fs::symlink_metadata(&file_path) {
                                Ok(metadata) if !metadata.is_file() || metadata.len() != 0 => {
                                    Err(Status::failed_precondition(
                                        "block target must be an empty regular file",
                                    ))
                                }
                                Ok(_) => Ok(()),
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    OpenOptions::new()
                                        .write(true)
                                        .create_new(true)
                                        .mode(0o600)
                                        .custom_flags(nix::libc::O_NOFOLLOW)
                                        .open(&file_path)
                                        .map_err(|e| {
                                            Status::internal(format!("create block target: {e}"))
                                        })?;
                                    Ok(())
                                }
                                Err(error) => {
                                    Err(Status::internal(format!("inspect block target: {error}")))
                                }
                            }
                        })
                        .await?;
                        volume.device.path.clone()
                    }
                }
            } else {
                staging
            };
            prepared = true;
            if let Some(mount) = existing_mount {
                self.verify_mount(&volume, mount, &target).await?;
                self.host.remount_bind(&target, &publish_flags).await?;
            } else {
                self.host.bind(&source, &target, &publish_flags).await?;
            }
            let mounts = self.host.mounts().await?;
            let mount = unique_mount(&mounts, &target)?
                .ok_or_else(|| Status::internal("publish command succeeded without mount"))?;
            self.verify_mount(&volume, mount, &target).await?;
            Self::verify_mount_flags(mount, &publish_flags)
        }
        .await;
        if let Err(error) = effect {
            let rollback = async {
                if registered
                    || self
                        .host
                        .mounts()
                        .await?
                        .iter()
                        .any(|mount| mount.target.starts_with(&target))
                {
                    return Err(Status::aborted("publication recovery is incomplete"));
                }
                if prepared {
                    self.remove_publish_target(&volume, &target).await?;
                }
                let mut next = registry.clone();
                next.volumes
                    .get_mut(&request.volume_id)
                    .expect("registered volume")
                    .published
                    .remove(&target);
                self.update(&mut registry, next).await
            }
            .await;
            return Err(if rollback.is_ok() {
                error
            } else {
                Status::aborted("publication effect requires retry or cleanup")
            });
        }
        Ok(Response::new(csi::NodePublishVolumeResponse {}))
    }

    async fn unpublish_volume(
        &self,
        request: Request<csi::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<csi::NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.lexical_path(&request.target_path)?;
        let mut registry = self.registry.lock().await.clone();
        let mounts = self.host.mounts().await?;
        let mounted = unique_mount(&mounts, &target)?;
        self.cleanup_path(&target, mounted.is_some()).await?;
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .filter(|v| v.published.contains_key(&target));
        if mounts
            .iter()
            .any(|m| m.target != target && m.target.starts_with(&target))
        {
            return Err(Status::failed_precondition(
                "publish target contains nested mounts",
            ));
        }
        if let Some(mount) = mounted {
            let volume = volume.ok_or_else(|| {
                Status::failed_precondition("refusing to unmount an unregistered target")
            })?;
            self.device_for_cleanup(volume).await?;
            self.verify_mount(volume, mount, &target).await?;
            self.host.unmount(&target).await?;
            if self
                .host
                .mounts()
                .await?
                .iter()
                .any(|mount| mount.target.starts_with(&target))
            {
                return Err(Status::internal("target remains mounted"));
            }
        }
        if let Some(volume) = volume {
            // Keep the durable publication intent until deletion succeeds so
            // a retry can finish cleanup after an unmount or process restart.
            self.remove_publish_target(volume, &target).await?;
            let mut next = registry.clone();
            next.volumes
                .get_mut(&request.volume_id)
                .expect("registered volume")
                .published
                .remove(&target);
            self.update(&mut registry, next).await?;
        }
        Ok(Response::new(csi::NodeUnpublishVolumeResponse {}))
    }

    async fn unstage_volume(
        &self,
        request: Request<csi::NodeUnstageVolumeRequest>,
    ) -> Result<Response<csi::NodeUnstageVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        let target = self.lexical_path(&request.staging_target_path)?;
        let mut registry = self.registry.lock().await.clone();
        let mounts = self.host.mounts().await?;
        let mounted = unique_mount(&mounts, &target)?;
        self.cleanup_path(&target, mounted.is_some()).await?;
        if let Some(volume) = registry.volumes.get(&request.volume_id).cloned() {
            if volume.staging_path != target {
                return Err(Status::invalid_argument(
                    "staging path differs from registered volume",
                ));
            }
            for path in volume.published.keys() {
                if mounts.iter().any(|mount| mount.target.starts_with(path)) {
                    return Err(Status::failed_precondition(
                        "volume still has published targets",
                    ));
                }
                self.remove_publish_target(&volume, path).await?;
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
                self.device_for_cleanup(&volume).await?;
                self.verify_mount(&volume, mount, &target).await?;
                self.host.unmount(&target).await?;
                if unique_mount(&self.host.mounts().await?, &target)?.is_some() {
                    return Err(Status::internal("staging path remains mounted"));
                }
            }
            self.host.ensure_released(&volume.device).await?;
            match self.host.discover(&volume.serial).await {
                Ok(device) if device != volume.device => self.host.ensure_released(&device).await?,
                Ok(_) => (),
                Err(error) if error.code() == tonic::Code::Unavailable => (),
                Err(error) => return Err(error),
            }
            let mut next = registry.clone();
            next.volumes.remove(&request.volume_id);
            self.update(&mut registry, next).await?;
        } else if mounted.is_some() || mounts.iter().any(|m| m.target.starts_with(&target)) {
            return Err(Status::failed_precondition(
                "refusing to unstage an unregistered mount",
            ));
        }
        Ok(Response::new(csi::NodeUnstageVolumeResponse {}))
    }

    async fn get_info(
        &self,
        _request: Request<csi::NodeGetInfoRequest>,
    ) -> Result<Response<csi::NodeGetInfoResponse>, Status> {
        Ok(Response::new(csi::NodeGetInfoResponse {
            node_id: self.config.node_id.clone(),
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn get_capabilities(
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

    async fn get_volume_stats(
        &self,
        request: Request<csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<csi::NodeGetVolumeStatsResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        if request.volume_path.is_empty() {
            return Err(Status::invalid_argument("volume_path is required"));
        }
        let registry = self.registry.lock().await.clone();
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::not_found("volume is not staged"))?;
        let target = self.path(&request.volume_path).await.map_err(|error| {
            if error.code() == tonic::Code::InvalidArgument {
                Status::not_found("volume is not mounted at the supplied path")
            } else {
                error
            }
        })?;
        if !request.staging_target_path.is_empty()
            && self.path(&request.staging_target_path).await? != volume.staging_path
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
                let stat = self.host.usage(&target).await?;
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

    async fn expand_volume(
        &self,
        request: Request<csi::NodeExpandVolumeRequest>,
    ) -> Result<Response<csi::NodeExpandVolumeResponse>, Status> {
        let request = request.into_inner();
        validate_volume_id(&request.volume_id)?;
        if request.volume_path.is_empty() {
            return Err(Status::invalid_argument("volume_path is required"));
        }
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
        let registry = self.registry.lock().await.clone();
        let volume = registry
            .volumes
            .get(&request.volume_id)
            .ok_or_else(|| Status::not_found("volume is not staged"))?;
        let target = self.path(&request.volume_path).await.map_err(|error| {
            if error.code() == tonic::Code::InvalidArgument {
                Status::not_found("volume is not mounted at the supplied path")
            } else {
                error
            }
        })?;
        self.checked_volume_path(volume, &target).await?;
        if !request.staging_target_path.is_empty()
            && self.path(&request.staging_target_path).await? != volume.staging_path
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
            let desired = spec(Some(&capability), &context)?;
            if desired.mode != volume.spec.mode || desired.access_mode != volume.spec.access_mode {
                return Err(Status::invalid_argument(
                    "expand capability differs from staged volume",
                ));
            }
            if desired.flags.iter().any(|flag| flag == "ro") {
                return Err(Status::failed_precondition(
                    "cannot expand a volume requested read-only",
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
            return Err(Status::unavailable(
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

#[tonic::async_trait]
impl csi::node_server::Node for NodeService {
    async fn node_stage_volume(
        &self,
        request: Request<csi::NodeStageVolumeRequest>,
    ) -> Result<Response<csi::NodeStageVolumeResponse>, Status> {
        self.run(
            "NodeStageVolume",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.stage_volume(request).await },
        )
        .await
    }
    async fn node_publish_volume(
        &self,
        request: Request<csi::NodePublishVolumeRequest>,
    ) -> Result<Response<csi::NodePublishVolumeResponse>, Status> {
        self.run(
            "NodePublishVolume",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.publish_volume(request).await },
        )
        .await
    }
    async fn node_unpublish_volume(
        &self,
        request: Request<csi::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<csi::NodeUnpublishVolumeResponse>, Status> {
        self.run(
            "NodeUnpublishVolume",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.unpublish_volume(request).await },
        )
        .await
    }
    async fn node_unstage_volume(
        &self,
        request: Request<csi::NodeUnstageVolumeRequest>,
    ) -> Result<Response<csi::NodeUnstageVolumeResponse>, Status> {
        self.run(
            "NodeUnstageVolume",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.unstage_volume(request).await },
        )
        .await
    }
    async fn node_get_info(
        &self,
        request: Request<csi::NodeGetInfoRequest>,
    ) -> Result<Response<csi::NodeGetInfoResponse>, Status> {
        self.get_info(request).await
    }
    async fn node_get_capabilities(
        &self,
        request: Request<csi::NodeGetCapabilitiesRequest>,
    ) -> Result<Response<csi::NodeGetCapabilitiesResponse>, Status> {
        self.get_capabilities(request).await
    }
    async fn node_get_volume_stats(
        &self,
        request: Request<csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<csi::NodeGetVolumeStatsResponse>, Status> {
        self.run(
            "NodeGetVolumeStats",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.get_volume_stats(request).await },
        )
        .await
    }
    async fn node_expand_volume(
        &self,
        request: Request<csi::NodeExpandVolumeRequest>,
    ) -> Result<Response<csi::NodeExpandVolumeResponse>, Status> {
        self.run(
            "NodeExpandVolume",
            request.get_ref().volume_id.clone(),
            move |service| async move { service.expand_volume(request).await },
        )
        .await
    }
}
