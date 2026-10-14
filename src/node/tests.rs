use super::*;
use crate::csi::node_server::Node;
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct Machine {
    mounts: Vec<Mount>,
    commands: Vec<String>,
    discoveries: Vec<String>,
    fail_after_bind: bool,
    capacity: i64,
    filesystem_capacity: i64,
    fail_grow: bool,
    capacity_after_rescan: Option<i64>,
    device_missing: bool,
    device_ambiguous: bool,
    replacement: Option<Device>,
    block_mounts: BTreeMap<PathBuf, (u64, u64)>,
    readonly: bool,
    devices: BTreeMap<String, Device>,
    fail_mount: bool,
    fail_bind: bool,
    omitted_flags: BTreeSet<String>,
    release_busy: bool,
    inaccessible_paths: BTreeSet<PathBuf>,
    paused_mounts: BTreeMap<PathBuf, Arc<Pause>>,
    device_filesystem: Option<(u64, u64)>,
    mount_barrier: Option<(Arc<tokio::sync::Barrier>, usize)>,
}

#[derive(Default)]
struct Pause {
    entered: Notify,
    resume: Notify,
}

struct FakeHost {
    device: Device,
    fs_type: Option<String>,
    machine: StdMutex<Machine>,
}

#[async_trait]
impl Host for FakeHost {
    async fn discover(&self, serial: &str) -> Result<Device, Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.discoveries.push(serial.into());
        if machine.device_ambiguous {
            return Err(Status::failed_precondition("device serial ambiguous"));
        }
        if let Some(device) = machine.devices.get(serial) {
            return Ok(device.clone());
        }
        if serial != "volume-serial" || machine.device_missing {
            return Err(Status::unavailable("device absent"));
        }
        Ok(machine
            .replacement
            .clone()
            .unwrap_or_else(|| self.device.clone()))
    }
    async fn filesystem(&self, _: &Device) -> Result<Option<String>, Status> {
        Ok(self.fs_type.clone())
    }
    async fn mounts(&self) -> Result<Vec<Mount>, Status> {
        let (mounts, barrier) = {
            let mut machine = self.machine.lock().unwrap();
            let barrier = machine.mount_barrier.as_mut().map(|(barrier, remaining)| {
                *remaining -= 1;
                barrier.clone()
            });
            if machine
                .mount_barrier
                .as_ref()
                .is_some_and(|(_, remaining)| *remaining == 0)
            {
                machine.mount_barrier = None;
            }
            (machine.mounts.clone(), barrier)
        };
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        Ok(mounts)
    }
    async fn mount_fs(
        &self,
        device: &Device,
        target: &Path,
        fs_type: &str,
        flags: &[String],
    ) -> Result<(), Status> {
        let pause = self.machine.lock().unwrap().paused_mounts.remove(target);
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.resume.notified().await;
        }
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("mount_fs".into());
        if machine.fail_mount {
            return Err(Status::internal("mount rejected before effect"));
        }
        let options = flags
            .iter()
            .filter(|flag| !machine.omitted_flags.contains(*flag))
            .cloned()
            .collect();
        machine.mounts.push(Mount {
            target: target.into(),
            root: "/".into(),
            major: device.major,
            minor: device.minor,
            fs_type: fs_type.into(),
            options,
        });
        Ok(())
    }
    async fn bind(&self, source: &Path, target: &Path, flags: &[String]) -> Result<(), Status> {
        {
            let mut machine = self.machine.lock().unwrap();
            if machine.fail_bind {
                return Err(Status::internal("bind rejected before effect"));
            }
            let device = machine
                .devices
                .values()
                .find(|device| device.path == source)
                .cloned()
                .or_else(|| {
                    machine
                        .mounts
                        .iter()
                        .find(|mount| mount.target == source)
                        .map(|mount| Device {
                            path: PathBuf::new(),
                            major: mount.major,
                            minor: mount.minor,
                        })
                })
                .or_else(|| machine.replacement.clone())
                .unwrap_or_else(|| self.device.clone());
            let block = source == device.path;
            if block {
                machine
                    .block_mounts
                    .insert(target.into(), (device.major, device.minor));
            }
            machine.commands.push("bind".into());
            machine.mounts.push(Mount {
                target: target.into(),
                root: "/".into(),
                major: if block { 0 } else { device.major },
                minor: if block { 42 } else { device.minor },
                fs_type: if block { "devtmpfs" } else { "xfs" }.into(),
                options: BTreeSet::from(["rw".into()]),
            });
            if machine.fail_after_bind {
                machine.fail_after_bind = false;
                return Err(Status::internal(
                    "simulated interruption between bind and remount",
                ));
            }
        }
        self.remount_bind(target, flags).await
    }
    async fn remount_bind(&self, target: &Path, flags: &[String]) -> Result<(), Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("remount_bind".into());
        let options = flags
            .iter()
            .filter(|flag| !machine.omitted_flags.contains(*flag))
            .cloned()
            .collect();
        machine
            .mounts
            .iter_mut()
            .find(|mount| mount.target == target)
            .unwrap()
            .options = options;
        Ok(())
    }
    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("unmount".into());
        machine.mounts.retain(|mount| mount.target != target);
        machine.block_mounts.remove(target);
        Ok(())
    }
    async fn grow(&self, device: &Device, target: &Path, _: &str) -> Result<(), Status> {
        let mut machine = self.machine.lock().unwrap();
        let mount = unique_mount(&machine.mounts, target)?.expect("growth needs a mounted FS");
        assert_eq!((mount.major, mount.minor), (device.major, device.minor));
        assert!(
            !mount.options.contains("ro"),
            "never grow a read-only mount"
        );
        machine.commands.push("grow".into());
        if machine.fail_grow {
            return Err(Status::internal("simulated filesystem growth failure"));
        }
        machine.filesystem_capacity = machine.capacity;
        Ok(())
    }
    async fn capacity(&self, _: &Device) -> Result<i64, Status> {
        Ok(self.machine.lock().unwrap().capacity)
    }
    async fn readonly(&self, _: &Device) -> Result<bool, Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("getro".into());
        Ok(machine.readonly)
    }
    async fn rescan(&self, device: &Device, serial: &str) -> Result<(), Status> {
        assert_eq!(device, &self.device);
        assert_eq!(serial, "volume-serial");
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("rescan".into());
        if let Some(capacity) = machine.capacity_after_rescan {
            machine.capacity = capacity;
        }
        Ok(())
    }
    async fn ensure_released(&self, device: &Device) -> Result<(), Status> {
        let machine = self.machine.lock().unwrap();
        if machine.release_busy
            || machine
                .mounts
                .iter()
                .any(|mount| (mount.major, mount.minor) == (device.major, device.minor))
        {
            return Err(Status::aborted("device is still held by a filesystem"));
        }
        Ok(())
    }
    async fn device_filesystem(&self, _: &Device) -> Result<Option<(u64, u64)>, Status> {
        Ok(self.machine.lock().unwrap().device_filesystem)
    }
    async fn block_identity(&self, path: &Path) -> Result<Option<(u64, u64)>, Status> {
        let machine = self.machine.lock().unwrap();
        if machine.inaccessible_paths.contains(path) {
            return Err(Status::internal("unrelated filesystem unavailable"));
        }
        Ok(machine.block_mounts.get(path).copied())
    }
    async fn usage(&self, _: &Path) -> Result<host::Usage, Status> {
        Ok(host::Usage {
            total: 1024,
            available: 512,
            used: 512,
            ..Default::default()
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    config: NodeConfig,
    host: Arc<FakeHost>,
}

impl Fixture {
    fn new(fs_type: Option<&str>) -> Self {
        // Do not allocate test artifacts in /tmp (tmpfs in the development VM).
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("node-tests");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("node-")
            .tempdir_in(base)
            .unwrap();
        let config = NodeConfig {
            node_id: "vm-incarnation-123".into(),
            state_dir: directory.path().join("state"),
            device_root: directory.path().join("dev"),
            sys_block_dir: directory.path().join("sys/block"),
            kubelet_root: directory.path().join("kubelet"),
        };
        for path in [
            &config.device_root,
            &config.sys_block_dir,
            &config.kubelet_root,
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let host = Arc::new(FakeHost {
            device: Device {
                path: config.device_root.join("vdb"),
                major: 8,
                minor: 16,
            },
            fs_type: fs_type.map(str::to_owned),
            machine: StdMutex::new(Machine {
                capacity: 1024 * 1024 * 1024,
                filesystem_capacity: 1024 * 1024 * 1024,
                ..Default::default()
            }),
        });
        Self {
            _directory: directory,
            config,
            host,
        }
    }
    fn service(&self) -> NodeService {
        let mut service = NodeService::new(self.config.clone()).unwrap();
        service.host = self.host.clone();
        service
    }
    fn stage_path(&self) -> String {
        self.config
            .kubelet_root
            .join("plugins/volume/globalmount")
            .to_string_lossy()
            .into()
    }
    fn target_path(&self) -> String {
        self.config
            .kubelet_root
            .join("pods/pod/volume/mount")
            .to_string_lossy()
            .into()
    }
    fn context(&self, block: bool) -> HashMap<String, String> {
        let mut context = HashMap::from([
            ("node_id".into(), self.config.node_id.clone()),
            ("device_serial".into(), "volume-serial".into()),
            ("format_policy".into(), "preformatted".into()),
            (
                "volume_mode".into(),
                if block { "block" } else { "filesystem" }.into(),
            ),
        ]);
        if !block {
            context.insert("fs_type".into(), "xfs".into());
        }
        context
    }
    fn capability(&self, block: bool) -> csi::VolumeCapability {
        csi::VolumeCapability {
            access_type: Some(if block {
                csi::volume_capability::AccessType::Block(csi::volume_capability::BlockVolume {})
            } else {
                csi::volume_capability::AccessType::Mount(csi::volume_capability::MountVolume {
                    fs_type: "xfs".into(),
                    mount_flags: vec![],
                    volume_mount_group: String::new(),
                })
            }),
            access_mode: Some(csi::volume_capability::AccessMode { mode: 1 }),
        }
    }
    fn mount_capability(&self, mount_flags: &[&str]) -> csi::VolumeCapability {
        let mut capability = self.capability(false);
        if let Some(csi::volume_capability::AccessType::Mount(mount)) = &mut capability.access_type
        {
            mount.mount_flags = mount_flags.iter().map(|flag| (*flag).into()).collect();
        }
        capability
    }
    fn stage(&self, block: bool) -> csi::NodeStageVolumeRequest {
        csi::NodeStageVolumeRequest {
            volume_id: "volume-1".into(),
            publish_context: self.context(block),
            staging_target_path: self.stage_path(),
            volume_capability: Some(self.capability(block)),
            ..Default::default()
        }
    }
    fn publish(&self, block: bool, readonly: bool) -> csi::NodePublishVolumeRequest {
        csi::NodePublishVolumeRequest {
            volume_id: "volume-1".into(),
            publish_context: self.context(block),
            staging_target_path: self.stage_path(),
            target_path: self.target_path(),
            volume_capability: Some(self.capability(block)),
            readonly,
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn adopted_blank_or_wrong_filesystem_is_never_formatted() {
    for signature in [None, Some("ext4"), Some("crypto_LUKS")] {
        let fixture = Fixture::new(signature);
        let service = fixture.service();
        let result = service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
        assert!(service.registry.lock().await.volumes.is_empty());
    }
}

#[tokio::test]
async fn filesystem_lifecycle_is_idempotent_across_process_restart() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    assert_eq!(
        fixture.host.machine.lock().unwrap().commands,
        ["mount_fs", "grow", "grow"]
    );
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .spec
            .flags
            .contains(&"nouuid".into())
    );
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    drop(service);
    let service = fixture.service();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    let stats = service
        .node_get_volume_stats(Request::new(csi::NodeGetVolumeStatsRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.target_path(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stats.usage.len(), 2);
    service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.target_path(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 1024,
                limit_bytes: 0,
            }),
            ..Default::default()
        }))
        .await
        .unwrap();
    let unstage = csi::NodeUnstageVolumeRequest {
        volume_id: "volume-1".into(),
        staging_target_path: fixture.stage_path(),
    };
    assert_eq!(
        service
            .node_unstage_volume(Request::new(unstage.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    for _ in 0..2 {
        service
            .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
                volume_id: "volume-1".into(),
                target_path: fixture.target_path(),
            }))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        service
            .node_unstage_volume(Request::new(unstage.clone()))
            .await
            .unwrap();
    }
    assert!(fixture.host.machine.lock().unwrap().mounts.is_empty());
    assert!(service.registry.lock().await.volumes.is_empty());
}

#[tokio::test]
async fn read_only_publish_recovers_interruption_between_bind_and_remount() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().fail_after_bind = true;
    assert!(
        service
            .node_publish_volume(Request::new(fixture.publish(false, true)))
            .await
            .is_err()
    );
    drop(service);
    let service = fixture.service();
    service
        .node_publish_volume(Request::new(fixture.publish(false, true)))
        .await
        .unwrap();
    let mounts = fixture.host.mounts().await.unwrap();
    let published = unique_mount(&mounts, Path::new(&fixture.target_path()))
        .unwrap()
        .unwrap();
    assert!(published.options.contains("ro"));
    assert!(published.options.contains("nodev"));
    assert!(published.options.contains("nosuid"));
}

#[tokio::test]
async fn foreign_mount_is_never_adopted_or_unmounted() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    fixture
        .host
        .mount_fs(
            &fixture.host.device,
            Path::new(&fixture.stage_path()),
            "xfs",
            &["rw".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        service
            .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
                volume_id: "volume-1".into(),
                staging_target_path: fixture.stage_path()
            }))
            .await
            .is_err()
    );
    assert!(
        service
            .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
                volume_id: "volume-1".into(),
                target_path: fixture.stage_path()
            }))
            .await
            .is_err()
    );
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"unmount".into())
    );
}

#[tokio::test]
async fn target_replaced_by_another_device_is_not_unmounted() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    fixture
        .host
        .machine
        .lock()
        .unwrap()
        .mounts
        .iter_mut()
        .find(|m| m.target == Path::new(&fixture.target_path()))
        .unwrap()
        .minor = 32;
    assert_eq!(
        service
            .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
                volume_id: "volume-1".into(),
                target_path: fixture.target_path()
            }))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"unmount".into())
    );
}

#[tokio::test]
async fn volume_identity_and_stage_publish_paths_are_exclusive() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    let mut second = fixture.stage(false);
    second.volume_id = "volume-2".into();
    second.staging_target_path = fixture
        .config
        .kubelet_root
        .join("different-stage")
        .to_string_lossy()
        .into();
    assert_eq!(
        service
            .node_stage_volume(Request::new(second))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::AlreadyExists
    );
    let mut publish = fixture.publish(false, false);
    publish.target_path = fixture.stage_path();
    assert_eq!(
        service
            .node_publish_volume(Request::new(publish))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::AlreadyExists
    );
}

#[tokio::test]
async fn raw_block_lifecycle_does_not_probe_or_mount_a_filesystem() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(true)))
        .await
        .unwrap();
    assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
    service
        .node_publish_volume(Request::new(fixture.publish(true, false)))
        .await
        .unwrap();
    let stats = service
        .node_get_volume_stats(Request::new(csi::NodeGetVolumeStatsRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.target_path(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stats.usage[0].total, 1024 * 1024 * 1024);
    let mounts = fixture.host.mounts().await.unwrap();
    assert!(!mounts[0].options.contains("nodev"));
    service
        .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
            volume_id: "volume-1".into(),
            target_path: fixture.target_path(),
        }))
        .await
        .unwrap();
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "volume-1".into(),
            staging_target_path: fixture.stage_path(),
        }))
        .await
        .unwrap();
    assert!(service.registry.lock().await.volumes.is_empty());
}

#[test]
fn process_lock_and_vm_incarnation_prevent_state_reuse() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    assert!(matches!(
        NodeService::new(fixture.config.clone()),
        Err(NodeError::Locked)
    ));
    drop(service);
    let mut other = fixture.config.clone();
    other.node_id = "different-vm".into();
    assert!(matches!(
        NodeService::new(other),
        Err(NodeError::Configuration(_))
    ));
}

#[tokio::test]
async fn paths_reject_escape_and_symlinks_and_flags_reject_mount_redirection() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    assert!(service.path("/etc/passwd").await.is_err());
    assert!(
        service
            .path(&format!(
                "{}/../escape",
                fixture.config.kubelet_root.display()
            ))
            .await
            .is_err()
    );
    std::os::unix::fs::symlink(
        &fixture.config.device_root,
        fixture.config.kubelet_root.join("link"),
    )
    .unwrap();
    assert!(
        service
            .path(&format!(
                "{}/link/target",
                fixture.config.kubelet_root.display()
            ))
            .await
            .is_err()
    );
    for flag in [
        "bind",
        "remount",
        "suid",
        "dev",
        "rw,bind",
        "loop",
        "offset=4096",
    ] {
        assert!(flags(&[flag.into()], false, false).is_err());
    }
    assert!(flags(&["rw".into(), "ro".into()], false, false).is_err());
}

#[tokio::test]
async fn raw_block_expands_from_unmounted_stage_after_verified_rescan() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(true)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().capacity_after_rescan = Some(2 * 1024 * 1024 * 1024);
    let response = service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.stage_path(),
            staging_target_path: fixture.stage_path(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 2 * 1024 * 1024 * 1024,
                limit_bytes: 0,
            }),
            volume_capability: Some(fixture.capability(true)),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.capacity_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(fixture.host.machine.lock().unwrap().commands, ["rescan"]);
}

#[tokio::test]
async fn expansion_does_not_claim_success_when_capacity_is_still_old() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().commands.clear();
    let error = service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.stage_path(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 2 * 1024 * 1024 * 1024,
                limit_bytes: 0,
            }),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"grow".into())
    );
}

#[tokio::test]
async fn hot_unplug_allows_cleanup_of_only_the_recorded_mounts() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().device_missing = true;
    service
        .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
            volume_id: "volume-1".into(),
            target_path: fixture.target_path(),
        }))
        .await
        .unwrap();
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "volume-1".into(),
            staging_target_path: fixture.stage_path(),
        }))
        .await
        .unwrap();
    assert!(fixture.host.machine.lock().unwrap().mounts.is_empty());
}

#[tokio::test]
async fn cleanup_refuses_ambiguous_serial_and_foreign_mount_even_after_unplug() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    let request = csi::NodeUnpublishVolumeRequest {
        volume_id: "volume-1".into(),
        target_path: fixture.target_path(),
    };
    fixture.host.machine.lock().unwrap().device_ambiguous = true;
    assert_eq!(
        service
            .node_unpublish_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.device_ambiguous = false;
        machine.device_missing = true;
        machine
            .mounts
            .iter_mut()
            .find(|mount| mount.target == Path::new(&fixture.target_path()))
            .unwrap()
            .minor = 32;
    }
    assert_eq!(
        service
            .node_unpublish_volume(Request::new(request))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"unmount".into())
    );
}

#[tokio::test]
async fn read_only_stage_remains_read_only_when_publish_flag_is_false() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let mut stage = fixture.stage(false);
    stage
        .publish_context
        .insert("readonly".into(), "true".into());
    stage
        .volume_capability
        .as_mut()
        .unwrap()
        .access_mode
        .as_mut()
        .unwrap()
        .mode = 2;
    service
        .node_stage_volume(Request::new(stage))
        .await
        .unwrap();
    let mut publish = fixture.publish(false, false);
    publish
        .publish_context
        .insert("readonly".into(), "true".into());
    publish
        .volume_capability
        .as_mut()
        .unwrap()
        .access_mode
        .as_mut()
        .unwrap()
        .mode = 2;
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    publish.readonly = true;
    service
        .node_publish_volume(Request::new(publish))
        .await
        .unwrap();
    for mount in &fixture.host.machine.lock().unwrap().mounts {
        assert!(mount.options.contains("ro"));
    }
}

#[tokio::test]
async fn opaque_volume_handle_with_slashes_and_spaces_survives_restart() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let mut request = fixture.stage(false);
    request.volume_id = "ceph/pool/opaque volume handle".into();
    service
        .node_stage_volume(Request::new(request.clone()))
        .await
        .unwrap();
    drop(service);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(request))
        .await
        .unwrap();
    assert!(
        service
            .registry
            .lock()
            .await
            .volumes
            .contains_key("ceph/pool/opaque volume handle")
    );
}

#[tokio::test]
async fn expansion_validates_supplied_ranges_and_capabilities_before_growth() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().commands.clear();
    for (required_bytes, limit_bytes) in [(0, 0), (-1, 0), (0, -1), (20, 10)] {
        let error = service
            .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
                volume_id: "volume-1".into(),
                volume_path: fixture.stage_path(),
                capacity_range: Some(csi::CapacityRange {
                    required_bytes,
                    limit_bytes,
                }),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }
    let error = service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.stage_path(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 2 * 1024 * 1024 * 1024,
                limit_bytes: 0,
            }),
            volume_capability: Some(fixture.capability(true)),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"rescan".into())
    );
    assert!(
        !fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"grow".into())
    );
    service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.stage_path(),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(
        fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .contains(&"grow".into())
    );
}

#[test]
fn serial_contract_is_exact_ascii_and_bounded_for_qemu() {
    for serial in ["a", "A_z-9.0", &"a".repeat(36)] {
        validate_serial(serial).unwrap();
    }
    for serial in ["", " abc", "abc ", "a/b", "a:b", "é", &"a".repeat(37)] {
        assert_eq!(
            validate_serial(serial).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}

#[tokio::test]
async fn reboot_reenumeration_reconciles_unmounted_devices_and_persists_the_new_identity() {
    for block in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(block)))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(fixture.publish(block, false)))
            .await
            .unwrap();
        let replacement = Device {
            path: fixture.config.device_root.join("sdc"),
            major: 8,
            minor: 32,
        };
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            machine.mounts.clear();
            machine.block_mounts.clear();
            machine.replacement = Some(replacement.clone());
        }
        drop(service);
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(block)))
            .await
            .unwrap();
        // Unmounted publication intent remains available for kubelet retry.
        assert_eq!(
            service.registry.lock().await.volumes["volume-1"]
                .published
                .len(),
            1
        );
        drop(service);
        let service = fixture.service();
        assert_eq!(
            service.registry.lock().await.volumes["volume-1"].device,
            replacement
        );
        service
            .node_publish_volume(Request::new(fixture.publish(block, false)))
            .await
            .unwrap();
        service
            .node_get_volume_stats(Request::new(csi::NodeGetVolumeStatsRequest {
                volume_id: "volume-1".into(),
                volume_path: fixture.target_path(),
                ..Default::default()
            }))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn reenumeration_never_rebinds_old_new_foreign_or_nested_mounts() {
    for scenario in 0..8 {
        let fixture = Fixture::new(None);
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(true)))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(fixture.publish(true, false)))
            .await
            .unwrap();
        let old = fixture.host.device.clone();
        let replacement = Device {
            path: fixture.config.device_root.join("sdc"),
            major: 8,
            minor: 32,
        };
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            machine.mounts.clear();
            machine.block_mounts.clear();
            let target = match scenario {
                0 => PathBuf::from(fixture.stage_path()),
                1 => PathBuf::from(fixture.stage_path()).join("nested"),
                2 => PathBuf::from(fixture.target_path()),
                3 => PathBuf::from(fixture.target_path()).join("nested"),
                _ => fixture.config.kubelet_root.join("foreign"),
            };
            let (major, minor) = match scenario {
                4 => (8, 16),
                5 => (8, 32),
                _ => (0, 42),
            };
            if scenario >= 6 {
                machine
                    .block_mounts
                    .insert(target.clone(), (8, if scenario == 6 { 16 } else { 32 }));
            }
            machine.mounts.push(Mount {
                target,
                root: "/".into(),
                major,
                minor,
                fs_type: "devtmpfs".into(),
                options: BTreeSet::from(["rw".into()]),
            });
            machine.replacement = Some(replacement);
            machine.commands.clear();
        }
        assert_eq!(
            service
                .node_stage_volume(Request::new(fixture.stage(true)))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition,
            "scenario {scenario}"
        );
        assert_eq!(
            service.registry.lock().await.volumes["volume-1"].device,
            old
        );
        assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
    }
}

#[tokio::test]
async fn foreign_raw_block_bind_prevents_first_stage_even_with_devtmpfs_mountinfo() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    let target = fixture.config.kubelet_root.join("foreign-block");
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.block_mounts.insert(target.clone(), (8, 16));
        machine.mounts.push(Mount {
            target,
            root: "/vdb".into(),
            major: 0,
            minor: 42,
            fs_type: "devtmpfs".into(),
            options: BTreeSet::from(["rw".into()]),
        });
    }
    assert_eq!(
        service
            .node_stage_volume(Request::new(fixture.stage(true)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(service.registry.lock().await.volumes.is_empty());
    assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn mount_flag_readonly_is_preserved_without_backend_or_publish_readonly_flags() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let mut capability = fixture.capability(false);
    if let Some(csi::volume_capability::AccessType::Mount(mount)) = &mut capability.access_type {
        mount.mount_flags.push("ro".into());
    }
    let mut stage = fixture.stage(false);
    stage.volume_capability = Some(capability.clone());
    service
        .node_stage_volume(Request::new(stage))
        .await
        .unwrap();
    let mut publish = fixture.publish(false, false);
    publish.volume_capability = Some(capability);
    service
        .node_publish_volume(Request::new(publish))
        .await
        .unwrap();
    assert!(
        fixture
            .host
            .machine
            .lock()
            .unwrap()
            .mounts
            .iter()
            .all(|mount| mount.options.contains("ro"))
    );
    assert!(
        service.registry.lock().await.volumes["volume-1"].published
            [Path::new(&fixture.target_path())]
    );
}

#[tokio::test]
async fn requested_raw_block_readonly_does_not_modify_an_existing_rw_publication() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(true)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(true, false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().commands.clear();
    let mut readonly = fixture.publish(true, true);
    readonly.target_path += "-readonly";
    assert_eq!(
        service
            .node_publish_volume(Request::new(readonly))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    {
        let machine = fixture.host.machine.lock().unwrap();
        assert!(machine.commands.is_empty());
        assert_eq!(machine.mounts.len(), 1);
        assert!(machine.mounts[0].options.contains("rw"));
    }
    assert_eq!(
        service.registry.lock().await.volumes["volume-1"]
            .published
            .len(),
        1
    );
}

#[tokio::test]
async fn raw_block_readonly_requires_backend_and_guest_protection_before_publication() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    let mut stage = fixture.stage(true);
    stage
        .publish_context
        .insert("readonly".into(), "true".into());
    service
        .node_stage_volume(Request::new(stage))
        .await
        .unwrap();
    let mut publish = fixture.publish(true, false);
    publish
        .publish_context
        .insert("readonly".into(), "true".into());
    assert_eq!(
        service
            .node_publish_volume(Request::new(publish.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .published
            .is_empty()
    );
    assert!(!Path::new(&fixture.target_path()).exists());
    fixture.host.machine.lock().unwrap().readonly = true;
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    drop(service);
    let service = fixture.service();
    service
        .node_publish_volume(Request::new(publish))
        .await
        .unwrap();
    assert!(
        fixture.host.machine.lock().unwrap().mounts[0]
            .options
            .contains("ro")
    );
    assert!(
        service.registry.lock().await.volumes["volume-1"].published
            [Path::new(&fixture.target_path())]
    );
}

#[tokio::test]
async fn unpublish_removes_only_registered_empty_placeholders_and_retains_retry_intent() {
    for block in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(block)))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(fixture.publish(block, false)))
            .await
            .unwrap();
        let target = PathBuf::from(fixture.target_path());
        let obstructing = if block {
            target.clone()
        } else {
            target.join("unowned-data")
        };
        std::fs::write(&obstructing, b"must not be deleted").unwrap();
        let request = csi::NodeUnpublishVolumeRequest {
            volume_id: "volume-1".into(),
            target_path: fixture.target_path(),
        };
        assert_eq!(
            service
                .node_unpublish_volume(Request::new(request.clone()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(std::fs::read(&obstructing).unwrap(), b"must not be deleted");
        assert!(
            service.registry.lock().await.volumes["volume-1"]
                .published
                .contains_key(&target)
        );
        drop(service);
        if block {
            std::fs::write(&obstructing, b"").unwrap();
        } else {
            std::fs::remove_file(&obstructing).unwrap();
        }
        let service = fixture.service();
        service
            .node_unpublish_volume(Request::new(request.clone()))
            .await
            .unwrap();
        assert!(!target.exists());
        service
            .node_unpublish_volume(Request::new(request.clone()))
            .await
            .unwrap();
        // State no longer owns this path; an unrelated replacement is preserved.
        if block {
            std::fs::write(&target, b"").unwrap();
        } else {
            std::fs::create_dir(&target).unwrap();
        }
        service
            .node_unpublish_volume(Request::new(request))
            .await
            .unwrap();
        assert!(target.exists());
        service
            .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
                volume_id: "volume-1".into(),
                staging_target_path: fixture.stage_path(),
            }))
            .await
            .unwrap();
        assert!(Path::new(&fixture.stage_path()).is_dir());
    }
}

#[tokio::test]
async fn reboot_disk_permutations_restage_and_publish_every_volume_in_either_order() {
    for block in [false, true] {
        for count in [2, 3] {
            for reverse in [false, true] {
                let fixture = Fixture::new(Some("xfs"));
                let devices: Vec<Device> = (0..count)
                    .map(|index| Device {
                        path: fixture.config.device_root.join(format!("disk-{index}")),
                        major: 8,
                        minor: 16 * (index as u64 + 1),
                    })
                    .collect();
                let serials: Vec<String> =
                    (0..count).map(|index| format!("serial-{index}")).collect();
                fixture.host.machine.lock().unwrap().devices = serials
                    .iter()
                    .cloned()
                    .zip(devices.iter().cloned())
                    .collect();
                let service = fixture.service();
                let stages: Vec<_> = (0..count)
                    .map(|index| {
                        let mut req = fixture.stage(block);
                        req.volume_id = format!("volume-{index}");
                        req.staging_target_path = fixture
                            .config
                            .kubelet_root
                            .join(format!("plugins/volume-{index}/globalmount"))
                            .to_string_lossy()
                            .into();
                        req.publish_context
                            .insert("device_serial".into(), serials[index].clone());
                        req
                    })
                    .collect();
                let publications: Vec<_> = (0..count)
                    .map(|index| {
                        let mut req = fixture.publish(block, false);
                        req.volume_id = stages[index].volume_id.clone();
                        req.staging_target_path = stages[index].staging_target_path.clone();
                        req.target_path = fixture
                            .config
                            .kubelet_root
                            .join(format!("pods/pod-{index}/volume/mount"))
                            .to_string_lossy()
                            .into();
                        req.publish_context = stages[index].publish_context.clone();
                        req
                    })
                    .collect();
                for index in 0..count {
                    service
                        .node_stage_volume(Request::new(stages[index].clone()))
                        .await
                        .unwrap();
                    service
                        .node_publish_volume(Request::new(publications[index].clone()))
                        .await
                        .unwrap();
                }
                {
                    let mut machine = fixture.host.machine.lock().unwrap();
                    machine.mounts.clear();
                    machine.block_mounts.clear();
                    machine.devices = (0..count)
                        .map(|index| (serials[index].clone(), devices[(index + 1) % count].clone()))
                        .collect();
                }
                drop(service);
                let service = fixture.service();
                let mut order: Vec<usize> = (0..count).collect();
                if reverse {
                    order.reverse();
                }
                for index in order {
                    service
                        .node_stage_volume(Request::new(stages[index].clone()))
                        .await
                        .unwrap();
                    service
                        .node_publish_volume(Request::new(publications[index].clone()))
                        .await
                        .unwrap();
                }
                let registry = service.registry.lock().await;
                for index in 0..count {
                    assert_eq!(
                        registry.volumes[&stages[index].volume_id].device,
                        devices[(index + 1) % count]
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn unpublish_refuses_a_nested_foreign_mount_even_without_the_parent_mount() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine
            .mounts
            .retain(|mount| mount.target != Path::new(&fixture.target_path()));
        machine.mounts.push(Mount {
            target: PathBuf::from(fixture.target_path()).join("foreign"),
            root: "/".into(),
            major: 0,
            minor: 42,
            fs_type: "tmpfs".into(),
            options: BTreeSet::from(["rw".into()]),
        });
        machine.commands.clear();
    }
    assert_eq!(
        service
            .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
                volume_id: "volume-1".into(),
                target_path: fixture.target_path(),
            }))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
    assert!(Path::new(&fixture.target_path()).exists());
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .published
            .contains_key(Path::new(&fixture.target_path()))
    );
}

#[tokio::test]
async fn cancelled_stage_keeps_its_lock_while_other_volumes_make_progress() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let pause = Arc::new(Pause::default());
    fixture
        .host
        .machine
        .lock()
        .unwrap()
        .paused_mounts
        .insert(fixture.stage_path().into(), pause.clone());
    let running = tokio::spawn({
        let service = service.clone();
        let request = fixture.stage(false);
        async move { service.node_stage_volume(Request::new(request)).await }
    });
    pause.entered.notified().await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    assert_eq!(
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Aborted
    );
    let mut second = fixture.stage(false);
    second.volume_id = "volume-2".into();
    second.staging_target_path = fixture
        .config
        .kubelet_root
        .join("plugins/second/globalmount")
        .to_string_lossy()
        .into();
    second
        .publish_context
        .insert("device_serial".into(), "second-serial".into());
    fixture.host.machine.lock().unwrap().devices.insert(
        "second-serial".into(),
        Device {
            path: fixture.config.device_root.join("vdc"),
            major: 8,
            minor: 32,
        },
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        service.node_stage_volume(Request::new(second)),
    )
    .await
    .unwrap()
    .unwrap();
    pause.resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), service.drain())
        .await
        .unwrap();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    assert_eq!(
        fixture
            .host
            .machine
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|command| *command == "mount_fs")
            .count(),
        2
    );
}

#[tokio::test]
async fn final_stage_and_publish_errors_remove_intent_without_waiting_for_inverse_rpcs() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    fixture.host.machine.lock().unwrap().fail_mount = true;
    assert_eq!(
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Internal
    );
    assert!(service.registry.lock().await.volumes.is_empty());
    fixture.host.machine.lock().unwrap().fail_mount = false;
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().fail_bind = true;
    assert_eq!(
        service
            .node_publish_volume(Request::new(fixture.publish(false, false)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Internal
    );
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .published
            .is_empty()
    );
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "volume-1".into(),
            staging_target_path: fixture.stage_path(),
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_mount_flags_keep_uncertain_effects_retryable() {
    for flag in ["nosuid", "nodev", "noexec", "ro"] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        let mut stage = fixture.stage(false);
        if let Some(csi::volume_capability::AccessType::Mount(mount)) = stage
            .volume_capability
            .as_mut()
            .unwrap()
            .access_type
            .as_mut()
        {
            mount.mount_flags = vec!["noexec".into(), "ro".into()];
        }
        fixture
            .host
            .machine
            .lock()
            .unwrap()
            .omitted_flags
            .insert(flag.into());
        assert_eq!(
            service
                .node_stage_volume(Request::new(stage))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Aborted
        );
        assert!(
            service
                .registry
                .lock()
                .await
                .volumes
                .contains_key("volume-1")
        );
        service
            .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
                volume_id: "volume-1".into(),
                staging_target_path: fixture.stage_path(),
            }))
            .await
            .unwrap();
        assert!(service.registry.lock().await.volumes.is_empty());
    }
}

#[tokio::test]
async fn foreign_unavailable_filesystems_do_not_receive_stat_calls() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    for fs_type in ["nfs", "nfs4", "fuse.sshfs"] {
        let target = fixture.config.kubelet_root.join(fs_type);
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.inaccessible_paths.insert(target.clone());
        machine.mounts.push(Mount {
            target,
            root: "/".into(),
            major: 0,
            minor: 90,
            fs_type: fs_type.into(),
            options: BTreeSet::new(),
        });
    }
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
}

#[tokio::test]
async fn mount_options_can_change_only_after_old_mounts_are_gone() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    let mut changed = fixture.stage(false);
    if let Some(csi::volume_capability::AccessType::Mount(mount)) = changed
        .volume_capability
        .as_mut()
        .unwrap()
        .access_type
        .as_mut()
    {
        mount.mount_flags.push("noexec".into());
    }
    assert!(
        service
            .node_stage_volume(Request::new(changed.clone()))
            .await
            .is_err()
    );
    fixture.host.machine.lock().unwrap().mounts.clear();
    drop(service);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(changed))
        .await
        .unwrap();
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .spec
            .flags
            .contains(&"noexec".into())
    );
}

#[tokio::test]
async fn foreign_or_missing_incarnation_cannot_discover_a_colliding_disk() {
    for block in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        // A different volume is attached to this VM under the serial cached in
        // an earlier incarnation's VolumeAttachment, and has not been staged.
        fixture.host.machine.lock().unwrap().devices.insert(
            "reused-slot-0".into(),
            Device {
                path: fixture.config.device_root.join("sdc"),
                major: 8,
                minor: 32,
            },
        );
        let service = fixture.service();
        for (incarnation, expected) in [
            (None, tonic::Code::InvalidArgument),
            (Some(""), tonic::Code::InvalidArgument),
            (Some("invalid node id"), tonic::Code::InvalidArgument),
            (Some("vm-incarnation-old"), tonic::Code::FailedPrecondition),
        ] {
            let mut request = fixture.stage(block);
            request
                .publish_context
                .insert("device_serial".into(), "reused-slot-0".into());
            request.publish_context.remove("node_id");
            if let Some(incarnation) = incarnation {
                request
                    .publish_context
                    .insert("node_id".into(), incarnation.into());
            }
            assert_eq!(
                service
                    .node_stage_volume(Request::new(request))
                    .await
                    .unwrap_err()
                    .code(),
                expected
            );
        }
        {
            let machine = fixture.host.machine.lock().unwrap();
            assert!(machine.discoveries.is_empty());
            assert!(machine.commands.is_empty());
            assert!(machine.mounts.is_empty());
        }
        assert!(service.registry.lock().await.volumes.is_empty());
    }
}

#[tokio::test]
async fn publish_checks_incarnation_before_using_an_existing_stage() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.discoveries.clear();
        machine.commands.clear();
    }
    for (incarnation, expected) in [
        (None, tonic::Code::InvalidArgument),
        (Some(""), tonic::Code::InvalidArgument),
        (Some("invalid node id"), tonic::Code::InvalidArgument),
        (Some("vm-incarnation-old"), tonic::Code::FailedPrecondition),
    ] {
        let mut request = fixture.publish(false, false);
        request.publish_context.remove("node_id");
        if let Some(incarnation) = incarnation {
            request
                .publish_context
                .insert("node_id".into(), incarnation.into());
        }
        assert_eq!(
            service
                .node_publish_volume(Request::new(request))
                .await
                .unwrap_err()
                .code(),
            expected
        );
    }
    {
        let machine = fixture.host.machine.lock().unwrap();
        assert!(machine.discoveries.is_empty());
        assert!(machine.commands.is_empty());
        assert_eq!(machine.mounts.len(), 1);
    }
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .published
            .is_empty()
    );
}

#[tokio::test]
async fn harmless_mount_option_edits_preserve_staged_flags_until_unstage() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let mut original = fixture.stage(false);
    original.volume_capability = Some(fixture.mount_capability(&["noatime"]));
    service
        .node_stage_volume(Request::new(original))
        .await
        .unwrap();
    let mut changed = fixture.stage(false);
    changed.volume_capability = Some(fixture.mount_capability(&["discard"]));
    service
        .node_stage_volume(Request::new(changed.clone()))
        .await
        .unwrap();
    let mut publish = fixture.publish(false, false);
    publish.volume_capability = changed.volume_capability.clone();
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    // kubelet supplies the edited PV capability to NodeExpand too. Mount flags
    // do not change an expansion's shape and must not cause an infeasible error.
    service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: "volume-1".into(),
            volume_path: fixture.stage_path(),
            volume_capability: Some(fixture.mount_capability(&["discard", "noexec"])),
            ..Default::default()
        }))
        .await
        .unwrap();
    drop(service);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(changed.clone()))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    let mut second_pod = publish.clone();
    second_pod.target_path += "-second";
    service
        .node_publish_volume(Request::new(second_pod.clone()))
        .await
        .unwrap();
    for mount in &fixture.host.machine.lock().unwrap().mounts {
        assert!(mount.options.contains("noatime"));
        assert!(!mount.options.contains("discard"));
        assert!(!mount.options.contains("noexec"));
    }
    assert!(
        service.registry.lock().await.volumes["volume-1"]
            .spec
            .flags
            .contains(&"noatime".into())
    );
    // A VM reboot may also renumber the device. It still does not apply edited
    // non-security flags to a stage whose ownership remains registered.
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.mounts.clear();
        machine.replacement = Some(Device {
            path: fixture.config.device_root.join("sdc"),
            major: 8,
            minor: 32,
        });
    }
    drop(service);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(changed.clone()))
        .await
        .unwrap();
    for request in [publish.clone(), second_pod.clone()] {
        service
            .node_publish_volume(Request::new(request))
            .await
            .unwrap();
    }
    for mount in &fixture.host.machine.lock().unwrap().mounts {
        assert!(mount.options.contains("noatime"));
        assert!(!mount.options.contains("discard"));
    }
    for target_path in [publish.target_path, second_pod.target_path] {
        service
            .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
                volume_id: "volume-1".into(),
                target_path,
            }))
            .await
            .unwrap();
    }
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "volume-1".into(),
            staging_target_path: fixture.stage_path(),
        }))
        .await
        .unwrap();
    service
        .node_stage_volume(Request::new(changed))
        .await
        .unwrap();
    let machine = fixture.host.machine.lock().unwrap();
    let mount = unique_mount(&machine.mounts, Path::new(&fixture.stage_path()))
        .unwrap()
        .unwrap();
    assert!(mount.options.contains("discard"));
    assert!(!mount.options.contains("noatime"));
}

#[tokio::test]
async fn security_mount_option_edits_do_not_modify_live_mounts() {
    for flag in ["ro", "noexec"] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(fixture.publish(false, false)))
            .await
            .unwrap();
        let previous = fixture.host.machine.lock().unwrap().mounts.clone();
        fixture.host.machine.lock().unwrap().commands.clear();
        let mut stage = fixture.stage(false);
        stage.volume_capability = Some(fixture.mount_capability(&[flag]));
        assert_eq!(
            service
                .node_stage_volume(Request::new(stage.clone()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        let mut publish = fixture.publish(false, false);
        publish.volume_capability = stage.volume_capability;
        assert_eq!(
            service
                .node_publish_volume(Request::new(publish))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        let machine = fixture.host.machine.lock().unwrap();
        assert!(machine.commands.is_empty());
        assert_eq!(machine.mounts, previous);
    }
}

#[tokio::test]
async fn vanished_publication_recomputes_effective_readonly_after_restaging() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    let capability = fixture.mount_capability(&["ro"]);
    let mut stage = fixture.stage(false);
    stage.volume_capability = Some(capability.clone());
    service
        .node_stage_volume(Request::new(stage))
        .await
        .unwrap();
    let mut publish = fixture.publish(false, false);
    publish.volume_capability = Some(capability);
    service
        .node_publish_volume(Request::new(publish))
        .await
        .unwrap();
    assert!(
        service.registry.lock().await.volumes["volume-1"].published
            [Path::new(&fixture.target_path())]
    );
    // A reboot loses mounts but retains the stage and publication intents.
    fixture.host.machine.lock().unwrap().mounts.clear();
    drop(service);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    assert!(
        !service.registry.lock().await.volumes["volume-1"].published
            [Path::new(&fixture.target_path())]
    );
    let machine = fixture.host.machine.lock().unwrap();
    assert!(
        machine
            .mounts
            .iter()
            .all(|mount| mount.options.contains("rw"))
    );
}

#[tokio::test]
async fn publication_intent_cannot_change_while_target_or_descendant_is_mounted() {
    for descendant in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(fixture.publish(false, true)))
            .await
            .unwrap();
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            if descendant {
                machine
                    .mounts
                    .iter_mut()
                    .find(|mount| mount.target == Path::new(&fixture.target_path()))
                    .unwrap()
                    .target
                    .push("nested");
            }
            machine.commands.clear();
        }
        assert_eq!(
            service
                .node_publish_volume(Request::new(fixture.publish(false, false)))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::AlreadyExists
        );
        assert!(fixture.host.machine.lock().unwrap().commands.is_empty());
        assert!(
            service.registry.lock().await.volumes["volume-1"].published
                [Path::new(&fixture.target_path())]
        );
    }
}

#[tokio::test]
async fn restored_filesystem_is_grown_at_stage_and_failed_growth_is_retried() {
    for fs_type in ["xfs", "ext4"] {
        let fixture = Fixture::new(Some(fs_type));
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            machine.capacity = 50 * 1024 * 1024 * 1024;
            machine.filesystem_capacity = 10 * 1024 * 1024 * 1024;
            machine.fail_grow = true;
        }
        let service = fixture.service();
        let mut stage = fixture.stage(false);
        stage
            .publish_context
            .insert("fs_type".into(), fs_type.into());
        if let Some(csi::volume_capability::AccessType::Mount(mount)) = stage
            .volume_capability
            .as_mut()
            .unwrap()
            .access_type
            .as_mut()
        {
            mount.fs_type = fs_type.into();
        }
        assert_eq!(
            service
                .node_stage_volume(Request::new(stage.clone()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Aborted
        );
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            assert_eq!(machine.filesystem_capacity, 10 * 1024 * 1024 * 1024);
            assert_eq!(machine.mounts.len(), 1);
            machine.fail_grow = false;
        }
        // Retry after process restart must finish growth on the existing mount.
        drop(service);
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(stage))
            .await
            .unwrap();
        let machine = fixture.host.machine.lock().unwrap();
        assert_eq!(machine.filesystem_capacity, machine.capacity);
        assert_eq!(machine.commands, ["mount_fs", "grow", "grow"]);
    }
}

#[tokio::test]
async fn readonly_and_block_stages_never_attempt_filesystem_growth() {
    for block in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        {
            let mut machine = fixture.host.machine.lock().unwrap();
            machine.capacity = 50 * 1024 * 1024 * 1024;
            machine.filesystem_capacity = 10 * 1024 * 1024 * 1024;
            machine.fail_grow = true;
        }
        let service = fixture.service();
        let mut stage = fixture.stage(block);
        if !block {
            stage
                .publish_context
                .insert("readonly".into(), "true".into());
        }
        for _ in 0..2 {
            service
                .node_stage_volume(Request::new(stage.clone()))
                .await
                .unwrap();
        }
        let machine = fixture.host.machine.lock().unwrap();
        assert!(!machine.commands.contains(&"grow".into()));
        assert_eq!(machine.filesystem_capacity, 10 * 1024 * 1024 * 1024);
    }
}

#[tokio::test]
async fn disappeared_serial_does_not_reserve_a_reused_device_number() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.mounts.clear();
        machine.device_missing = true;
        machine
            .devices
            .insert("new-serial".into(), fixture.host.device.clone());
    }
    let mut stage = fixture.stage(false);
    stage.volume_id = "new-volume".into();
    stage.staging_target_path = fixture
        .config
        .kubelet_root
        .join("plugins/new/globalmount")
        .to_string_lossy()
        .into();
    stage
        .publish_context
        .insert("device_serial".into(), "new-serial".into());
    service
        .node_stage_volume(Request::new(stage))
        .await
        .unwrap();
    assert!(
        !service
            .registry
            .lock()
            .await
            .volumes
            .contains_key("volume-1")
    );
}

#[tokio::test]
async fn unstage_keeps_state_until_the_device_is_released() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().release_busy = true;
    let request = csi::NodeUnstageVolumeRequest {
        volume_id: "volume-1".into(),
        staging_target_path: fixture.stage_path(),
    };
    assert_eq!(
        service
            .node_unstage_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Aborted
    );
    assert!(fixture.host.machine.lock().unwrap().mounts.is_empty());
    drop(service);
    let service = fixture.service();
    assert!(
        service
            .registry
            .lock()
            .await
            .volumes
            .contains_key("volume-1")
    );
    assert_eq!(
        service
            .node_unstage_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Aborted
    );
    fixture.host.machine.lock().unwrap().release_busy = false;
    service
        .node_unstage_volume(Request::new(request))
        .await
        .unwrap();
    assert!(service.registry.lock().await.volumes.is_empty());
}

#[tokio::test]
async fn block_stats_accept_only_the_owned_kubelet_mapping() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(true)))
        .await
        .unwrap();
    let base = fixture
        .config
        .kubelet_root
        .join("plugins/kubernetes.io/csi/volumeDevices");
    let mut publish = fixture.publish(true, false);
    publish.target_path = base.join("publish/pv/pod-id").to_string_lossy().into();
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    let alias = base.join("pv/dev/pod-id");
    fixture
        .host
        .bind(Path::new(&publish.target_path), &alias, &[])
        .await
        .unwrap();
    fixture
        .host
        .machine
        .lock()
        .unwrap()
        .block_mounts
        .insert(alias.clone(), (8, 16));
    let request = csi::NodeGetVolumeStatsRequest {
        volume_id: "volume-1".into(),
        volume_path: alias.to_string_lossy().into(),
        ..Default::default()
    };
    assert_eq!(
        service
            .node_get_volume_stats(Request::new(request.clone()))
            .await
            .unwrap()
            .into_inner()
            .usage[0]
            .total,
        1024 * 1024 * 1024
    );
    fixture
        .host
        .machine
        .lock()
        .unwrap()
        .block_mounts
        .insert(alias, (8, 48));
    assert_eq!(
        service
            .node_get_volume_stats(Request::new(request))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
}

#[tokio::test]
async fn raw_bind_on_the_device_inode_filesystem_prevents_adoption() {
    let fixture = Fixture::new(None);
    let service = fixture.service();
    let target = fixture.config.kubelet_root.join("foreign-raw");
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.device_filesystem = Some((8, 1));
        machine.block_mounts.insert(target.clone(), (8, 16));
        machine.mounts.push(Mount {
            target,
            root: "/devices/volume".into(),
            major: 8,
            minor: 1,
            fs_type: "ext4".into(),
            options: BTreeSet::from(["rw".into()]),
        });
    }
    assert_eq!(
        service
            .node_stage_volume(Request::new(fixture.stage(true)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(service.registry.lock().await.volumes.is_empty());
}

#[tokio::test]
async fn concurrent_publications_reserve_overlapping_paths_before_creating_them() {
    for _ in 0..4 {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(false)))
            .await
            .unwrap();
        let mut second_stage = fixture.stage(false);
        second_stage.volume_id = "volume-2".into();
        second_stage.staging_target_path = fixture
            .config
            .kubelet_root
            .join("plugins/second/globalmount")
            .to_string_lossy()
            .into();
        second_stage
            .publish_context
            .insert("device_serial".into(), "second-serial".into());
        fixture.host.machine.lock().unwrap().devices.insert(
            "second-serial".into(),
            Device {
                path: fixture.config.device_root.join("vdc"),
                major: 8,
                minor: 32,
            },
        );
        service
            .node_stage_volume(Request::new(second_stage.clone()))
            .await
            .unwrap();
        let parent = fixture.publish(false, false);
        let child_path = Path::new(&parent.target_path).join("child");
        let mut child = parent.clone();
        child.volume_id = second_stage.volume_id;
        child.staging_target_path = second_stage.staging_target_path;
        child.publish_context = second_stage.publish_context;
        child.target_path = child_path.to_string_lossy().into();
        fixture.host.machine.lock().unwrap().mount_barrier =
            Some((Arc::new(tokio::sync::Barrier::new(2)), 4));
        let (parent_result, child_result) =
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                tokio::join!(
                    service.node_publish_volume(Request::new(parent)),
                    service.node_publish_volume(Request::new(child))
                )
            })
            .await
            .unwrap();
        assert_ne!(parent_result.is_ok(), child_result.is_ok());
        assert_eq!(
            child_path.exists(),
            child_result.is_ok(),
            "a refused child publication must not modify the parent volume"
        );
    }
}

#[tokio::test]
async fn failed_target_preparation_preserves_foreign_content_and_removes_intent() {
    for block in [false, true] {
        let fixture = Fixture::new(Some("xfs"));
        let service = fixture.service();
        service
            .node_stage_volume(Request::new(fixture.stage(block)))
            .await
            .unwrap();
        let target = PathBuf::from(fixture.target_path());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "unrelated data").unwrap();
        assert!(
            service
                .node_publish_volume(Request::new(fixture.publish(block, false)))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unrelated data");
        assert!(
            service.registry.lock().await.volumes["volume-1"]
                .published
                .is_empty()
        );
        service
            .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
                volume_id: "volume-1".into(),
                staging_target_path: fixture.stage_path(),
            }))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn missing_serial_does_not_bypass_release_of_the_recorded_device() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    {
        let mut machine = fixture.host.machine.lock().unwrap();
        machine.device_missing = true;
        machine.release_busy = true;
    }
    let request = csi::NodeUnstageVolumeRequest {
        volume_id: "volume-1".into(),
        staging_target_path: fixture.stage_path(),
    };
    assert_eq!(
        service
            .node_unstage_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Aborted
    );
    assert!(
        service
            .registry
            .lock()
            .await
            .volumes
            .contains_key("volume-1")
    );
    fixture.host.machine.lock().unwrap().release_busy = false;
    service
        .node_unstage_volume(Request::new(request))
        .await
        .unwrap();
}

#[tokio::test]
async fn reattached_serial_does_not_prevent_removing_its_old_owned_mounts() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(fixture.publish(false, false)))
        .await
        .unwrap();
    fixture.host.machine.lock().unwrap().replacement = Some(Device {
        path: fixture.config.device_root.join("vdc"),
        major: 8,
        minor: 32,
    });
    service
        .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
            volume_id: "volume-1".into(),
            target_path: fixture.target_path(),
        }))
        .await
        .unwrap();
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "volume-1".into(),
            staging_target_path: fixture.stage_path(),
        }))
        .await
        .unwrap();
    assert!(service.registry.lock().await.volumes.is_empty());
    service
        .node_stage_volume(Request::new(fixture.stage(false)))
        .await
        .unwrap();
    assert_eq!(
        service.registry.lock().await.volumes["volume-1"]
            .device
            .minor,
        32
    );
}
