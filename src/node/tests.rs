use super::*;
use crate::csi::node_server::Node;
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct Machine {
    mounts: Vec<Mount>,
    commands: Vec<String>,
    fail_after_bind: bool,
    capacity: i64,
    capacity_after_rescan: Option<i64>,
    device_missing: bool,
    device_ambiguous: bool,
}

struct FakeHost {
    device: Device,
    fs_type: Option<String>,
    machine: StdMutex<Machine>,
}

#[async_trait]
impl Host for FakeHost {
    async fn discover(&self, serial: &str) -> Result<Device, Status> {
        let machine = self.machine.lock().unwrap();
        if machine.device_ambiguous {
            return Err(Status::failed_precondition("device serial ambiguous"));
        }
        if serial != "volume-serial" || machine.device_missing {
            return Err(Status::unavailable("device absent"));
        }
        Ok(self.device.clone())
    }
    async fn filesystem(&self, _: &Device) -> Result<Option<String>, Status> {
        Ok(self.fs_type.clone())
    }
    async fn mounts(&self) -> Result<Vec<Mount>, Status> {
        Ok(self.machine.lock().unwrap().mounts.clone())
    }
    async fn mount_fs(
        &self,
        _: &Device,
        target: &Path,
        fs_type: &str,
        flags: &[String],
    ) -> Result<(), Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("mount_fs".into());
        machine.mounts.push(Mount {
            target: target.into(),
            root: "/".into(),
            major: 8,
            minor: 16,
            fs_type: fs_type.into(),
            options: flags.iter().cloned().collect(),
        });
        Ok(())
    }
    async fn bind(&self, _: &Path, target: &Path, flags: &[String]) -> Result<(), Status> {
        {
            let mut machine = self.machine.lock().unwrap();
            machine.commands.push("bind".into());
            machine.mounts.push(Mount {
                target: target.into(),
                root: "/".into(),
                major: 8,
                minor: 16,
                fs_type: "xfs".into(),
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
        machine
            .mounts
            .iter_mut()
            .find(|mount| mount.target == target)
            .unwrap()
            .options = flags.iter().cloned().collect();
        Ok(())
    }
    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        let mut machine = self.machine.lock().unwrap();
        machine.commands.push("unmount".into());
        machine.mounts.retain(|mount| mount.target != target);
        Ok(())
    }
    async fn grow(&self, _: &Device, _: &Path, _: &str) -> Result<(), Status> {
        self.machine.lock().unwrap().commands.push("grow".into());
        Ok(())
    }
    async fn capacity(&self, _: &Device) -> Result<i64, Status> {
        Ok(self.machine.lock().unwrap().capacity)
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
    fn block_identity(&self, _: &Path) -> Result<(u64, u64), Status> {
        Ok((8, 16))
    }
    fn usage(&self, _: &Path) -> Result<host::Usage, Status> {
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
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/node-tests");
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
    assert_eq!(fixture.host.machine.lock().unwrap().commands, ["mount_fs"]);
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

#[test]
fn paths_reject_escape_and_symlinks_and_flags_reject_mount_redirection() {
    let fixture = Fixture::new(Some("xfs"));
    let service = fixture.service();
    assert!(service.path("/etc/passwd").is_err());
    assert!(
        service
            .path(&format!(
                "{}/../escape",
                fixture.config.kubelet_root.display()
            ))
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
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
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
async fn read_only_attachment_cannot_be_published_writable() {
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
    assert_eq!(
        service
            .node_publish_volume(Request::new(publish.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
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
