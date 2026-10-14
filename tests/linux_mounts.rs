//! Real Linux mount/blkid/resize integration test, disabled by default.
//!
//! Run only in a disposable privileged container, with
//! CLEVER_CSI_PRIVILEGED_TESTS=1 and TMPDIR outside /tmp:
//! cargo test --test linux_mounts -- --ignored --nocapture
//!
//! The block devices and filesystems are real, newly allocated loop devices.
//! Only serial discovery uses a synthetic sysfs VPD80 fixture: this is NOT a
//! QEMU/Supernova attachment test. No existing storage device is formatted.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use clever_csi_driver::{
    csi::{self, node_server::Node},
    node::{NodeConfig, NodeService},
};
use sha2::{Digest, Sha256};
use tonic::Request;

const MIB: u64 = 1024 * 1024;

fn run(program: &str, args: &[&OsStr]) -> String {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn digest(path: &Path) -> Vec<u8> {
    let mut input = File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let count = input.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    hash.finalize().to_vec()
}

struct LoopDisk {
    device: PathBuf,
    backing: PathBuf,
    targets: Vec<PathBuf>,
    device_number: (u64, u64),
    ownership_marker: PathBuf,
    backing_identity: (u64, u64, u64),
}

impl LoopDisk {
    fn new(backing: PathBuf, bytes: u64) -> Self {
        Self::with_readonly(backing, bytes, false)
    }

    fn with_readonly(backing: PathBuf, bytes: u64, readonly: bool) -> Self {
        Self::with_marker(backing, bytes, readonly, None)
    }

    fn with_marker(backing: PathBuf, bytes: u64, readonly: bool, marker: Option<PathBuf>) -> Self {
        File::create(&backing).unwrap().set_len(bytes).unwrap();
        // Docker's private /dev does not receive host udev events. loop-control
        // can allocate an index whose block node is not yet visible here.
        let candidate = run("losetup", &["--find".as_ref()]);
        let index = candidate
            .strip_prefix("/dev/loop")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        if !Path::new(&candidate).exists() {
            nix::sys::stat::mknod(
                Path::new(&candidate),
                nix::sys::stat::SFlag::S_IFBLK,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
                nix::sys::stat::makedev(7, index),
            )
            .unwrap();
        }
        let mut args = vec!["--find".as_ref(), "--show".as_ref()];
        if readonly {
            args.push("--read-only".as_ref());
        }
        args.push(backing.as_os_str());
        let device = PathBuf::from(run("losetup", &args));
        // The only loop devices we touch are returned by this allocation.
        assert!(
            device.starts_with("/dev")
                && device
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("loop")
        );
        let stat = std::fs::metadata(&device).unwrap();
        // Survives SIGKILL so the outer Docker runner can detach only our loops.
        let ownership_marker = marker.unwrap_or_else(|| backing.with_extension("loop-owner"));
        let backing_metadata = std::fs::metadata(&backing).unwrap();
        let backing_identity = (
            nix::sys::stat::major(backing_metadata.dev()),
            nix::sys::stat::minor(backing_metadata.dev()),
            backing_metadata.ino(),
        );
        let mut marker = File::create(&ownership_marker).unwrap();
        writeln!(
            marker,
            "{}\n{}\n{}:{}\n{}",
            device.display(),
            backing.display(),
            backing_identity.0,
            backing_identity.1,
            backing_identity.2
        )
        .unwrap();
        marker.sync_all().unwrap();
        Self {
            device,
            backing,
            targets: vec![],
            device_number: (
                nix::sys::stat::major(stat.rdev()),
                nix::sys::stat::minor(stat.rdev()),
            ),
            ownership_marker,
            backing_identity,
        }
    }

    fn grow(&self, bytes: u64) {
        OpenOptions::new()
            .write(true)
            .open(&self.backing)
            .unwrap()
            .set_len(bytes)
            .unwrap();
        run(
            "losetup",
            &["--set-capacity".as_ref(), self.device.as_os_str()],
        );
    }

    fn mount_belongs_to_loop(&self, path: &Path) -> bool {
        let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        let Some(line) = mounts
            .lines()
            .find(|line| line.split_whitespace().nth(4) == path.to_str())
        else {
            return false;
        };
        let number = format!("{}:{}", self.device_number.0, self.device_number.1);
        if line.split_whitespace().nth(2) == Some(number.as_str()) {
            return true;
        }
        // Raw block bind mounts inherit devtmpfs' mount dev_t. The bound inode
        // must instead be the exact block device allocated by this test.
        std::fs::metadata(path).is_ok_and(|metadata| {
            (
                nix::sys::stat::major(metadata.rdev()),
                nix::sys::stat::minor(metadata.rdev()),
            ) == self.device_number
        })
    }
}

impl Drop for LoopDisk {
    fn drop(&mut self) {
        // A name alone is not ownership: an external process could detach and
        // reuse a loop number. Check its backing file before touching it again.
        let ownership = Command::new("losetup")
            .args([
                "--list",
                "--noheadings",
                "--output",
                "BACK-MAJ:MIN,BACK-INO",
            ])
            .arg(&self.device)
            .output();
        let owned = ownership.is_ok_and(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            let fields: Vec<_> = text.split_whitespace().collect();
            output.status.success()
                && fields.len() == 2
                && fields[0] == format!("{}:{}", self.backing_identity.0, self.backing_identity.1)
                && fields[1] == self.backing_identity.2.to_string()
        });
        if !owned {
            eprintln!(
                "test cleanup: {} no longer belongs to this test; leaving it untouched",
                self.device.display()
            );
            return;
        }
        for target in self.targets.iter().rev() {
            if self.mount_belongs_to_loop(target) {
                let result = Command::new("umount").arg("--").arg(target).status();
                if !result.is_ok_and(|status| status.success()) {
                    eprintln!("test cleanup could not unmount {}", target.display());
                }
            }
        }
        // losetup -d affects only this newly allocated device. A still-busy loop
        // becomes autoclear instead of forcing another mount off the device.
        let result = Command::new("losetup")
            .arg("--detach")
            .arg(&self.device)
            .status();
        if !result.is_ok_and(|status| status.success()) {
            eprintln!("test cleanup could not detach {}", self.device.display());
        } else {
            let _ = std::fs::remove_file(&self.ownership_marker);
        }
    }
}

fn fixture_serial(root: &Path, disk: &LoopDisk, serial: &str) {
    let entry = root.join(disk.device.file_name().unwrap());
    // The disk is still a loop. Only topology/serial lookup is synthetic;
    // actual mount, flush, block identity and exclusive-open checks are real.
    let topology = root.parent().unwrap().join("synthetic-topology");
    let transport = topology.join("virtio0");
    let device = transport
        .join("host0/target0:0:0")
        .join(disk.device.file_name().unwrap());
    for path in [
        &entry,
        &device,
        &topology.join("buses/scsi"),
        &topology.join("buses/virtio"),
        &topology.join("drivers/virtio_scsi"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    symlink(&device, entry.join("device")).unwrap();
    symlink(topology.join("buses/scsi"), device.join("subsystem")).unwrap();
    if !transport.join("driver").exists() {
        symlink(
            topology.join("drivers/virtio_scsi"),
            transport.join("driver"),
        )
        .unwrap();
        symlink(topology.join("buses/virtio"), transport.join("subsystem")).unwrap();
    }
    std::fs::write(device.join("type"), b"0\n").unwrap();
    std::fs::write(device.join("rescan"), b"").unwrap();
    std::fs::write(
        entry.join("dev"),
        format!("{}:{}\n", disk.device_number.0, disk.device_number.1),
    )
    .unwrap();
    let mut page = vec![0u8, 0x80];
    page.extend_from_slice(&(serial.len() as u16).to_be_bytes());
    page.extend_from_slice(serial.as_bytes());
    std::fs::write(entry.join("device/vpd_pg80"), page).unwrap();
}

fn capability(fs_type: Option<&str>) -> csi::VolumeCapability {
    csi::VolumeCapability {
        access_type: Some(match fs_type {
            Some(fs_type) => {
                csi::volume_capability::AccessType::Mount(csi::volume_capability::MountVolume {
                    fs_type: fs_type.into(),
                    ..Default::default()
                })
            }
            None => {
                csi::volume_capability::AccessType::Block(csi::volume_capability::BlockVolume {})
            }
        }),
        access_mode: Some(csi::volume_capability::AccessMode { mode: 1 }),
    }
}

fn context(serial: &str, fs_type: Option<&str>) -> HashMap<String, String> {
    let mut context = HashMap::from([
        ("node_id".into(), "vm-linux-integration-test".into()),
        ("device_serial".into(), serial.into()),
        ("format_policy".into(), "preformatted".into()),
        (
            "volume_mode".into(),
            if fs_type.is_some() {
                "filesystem"
            } else {
                "block"
            }
            .into(),
        ),
        ("readonly".into(), "false".into()),
    ]);
    if let Some(fs_type) = fs_type {
        context.insert("fs_type".into(), fs_type.into());
    }
    context
}

async fn lifecycle(root: &Path, fs_type: Option<&str>) {
    let name = fs_type.unwrap_or("block");
    let root = root.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let initial = if fs_type == Some("xfs") {
        384 * MIB
    } else {
        64 * MIB
    };
    let expanded = initial + 128 * MIB;
    let mut disk = LoopDisk::new(root.join("new-test-image.raw"), initial);
    match fs_type {
        Some("xfs") => {
            run("mkfs.xfs", &["-f".as_ref(), disk.device.as_os_str()]);
        }
        Some("ext4") => {
            run(
                "mkfs.ext4",
                &[
                    "-F".as_ref(),
                    "-q".as_ref(),
                    "-E".as_ref(),
                    "lazy_itable_init=0,lazy_journal_init=0".as_ref(),
                    disk.device.as_os_str(),
                ],
            );
        }
        None => (),
        _ => unreachable!(),
    }
    // A clone/restore may have a larger block device but retain its source's
    // smaller filesystem. Prepare real data in that filesystem before growing
    // only the backing device; the first NodeStage must finish filesystem growth.
    let restored_source = fs_type.map(|fs_type| {
        let source_mount = root.join("source-before-restore");
        std::fs::create_dir(&source_mount).unwrap();
        disk.targets.push(source_mount.clone());
        run(
            "mount",
            &[
                "-t".as_ref(),
                fs_type.as_ref(),
                disk.device.as_os_str(),
                source_mount.as_os_str(),
            ],
        );
        let source_payload = source_mount.join("restored-payload");
        let mut writer = File::create(&source_payload).unwrap();
        let payload: Vec<u8> = (0..MIB).map(|index| (index % 239) as u8).collect();
        writer.write_all(&payload).unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        let source_digest = digest(&source_payload);
        assert_eq!(source_digest, Sha256::digest(&payload).to_vec());
        let stat = nix::sys::statvfs::statvfs(&source_mount).unwrap();
        let source_capacity = stat.blocks() * stat.fragment_size();
        run("umount", &[source_mount.as_os_str()]);
        disk.grow(initial + 64 * MIB);
        (source_capacity, source_digest)
    });
    let config = NodeConfig {
        node_id: "vm-linux-integration-test".into(),
        state_dir: root.join("state"),
        device_root: "/dev".into(),
        sys_block_dir: root.join("synthetic-sysfs-block"),
        kubelet_root: root.join("kubelet"),
    };
    fixture_serial(&config.sys_block_dir, &disk, &format!("test-{name}"));
    let unrelated = config.sys_block_dir.join("unrelated/device");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(unrelated.join("vpd_pg80"), [0, 0x80, 0, 20, b'x']).unwrap();
    let stage = config.kubelet_root.join("plugins/test/globalmount");
    let target = config.kubelet_root.join("pods/test/volume/mount");
    disk.targets = vec![stage.clone(), target.clone()];
    let stage_request = csi::NodeStageVolumeRequest {
        volume_id: name.into(),
        staging_target_path: stage.to_string_lossy().into(),
        volume_capability: Some(capability(fs_type)),
        publish_context: context(&format!("test-{name}"), fs_type),
        ..Default::default()
    };
    let publish_request = csi::NodePublishVolumeRequest {
        volume_id: name.into(),
        staging_target_path: stage.to_string_lossy().into(),
        target_path: target.to_string_lossy().into(),
        volume_capability: Some(capability(fs_type)),
        publish_context: context(&format!("test-{name}"), fs_type),
        ..Default::default()
    };
    let service = NodeService::new(config.clone()).unwrap();
    if fs_type.is_none() {
        let foreign = root.join("foreign-block-bind");
        File::create(&foreign).unwrap();
        disk.targets.push(foreign.clone());
        run(
            "mount",
            &[
                "--bind".as_ref(),
                disk.device.as_os_str(),
                foreign.as_os_str(),
            ],
        );
        assert_eq!(
            service
                .node_stage_volume(Request::new(stage_request.clone()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert!(disk.mount_belongs_to_loop(&foreign));
        run("umount", &[foreign.as_os_str()]);
        std::fs::remove_file(foreign).unwrap();
    }
    service
        .node_stage_volume(Request::new(stage_request.clone()))
        .await
        .unwrap();
    if let Some((source_capacity, source_digest)) = &restored_source {
        let stat = nix::sys::statvfs::statvfs(&stage).unwrap();
        let staged_capacity = stat.blocks() * stat.fragment_size();
        assert!(
            staged_capacity > *source_capacity + 32 * MIB,
            "first stage must grow the restored {name} filesystem before any NodeExpand"
        );
        assert_eq!(digest(&stage.join("restored-payload")), *source_digest);
    }
    service
        .node_publish_volume(Request::new(publish_request.clone()))
        .await
        .unwrap();
    if let Some((source_capacity, source_digest)) = &restored_source {
        let stats = service
            .node_get_volume_stats(Request::new(csi::NodeGetVolumeStatsRequest {
                volume_id: name.into(),
                volume_path: target.to_string_lossy().into(),
                staging_target_path: stage.to_string_lossy().into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(stats.usage[0].total > (*source_capacity + 32 * MIB) as i64);
        assert_eq!(digest(&target.join("restored-payload")), *source_digest);
        eprintln!(
            "{name}: restored filesystem grew at first stage/publish with payload intact and no NodeExpand"
        );
    }
    if fs_type.is_none() {
        let mut unsafe_readonly = publish_request.clone();
        unsafe_readonly.readonly = true;
        unsafe_readonly.target_path += "-readonly";
        let rejected_target = PathBuf::from(&unsafe_readonly.target_path);
        assert_eq!(
            service
                .node_publish_volume(Request::new(unsafe_readonly))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert!(!rejected_target.exists());
        assert_eq!(
            run("blockdev", &["--getro".as_ref(), disk.device.as_os_str()]),
            "0"
        );
    }
    let payload: Vec<u8> = (0..MIB).map(|index| (index % 251) as u8).collect();
    let data_path = if fs_type.is_some() {
        target.join("verified-payload")
    } else {
        target.clone()
    };
    let mut writer = OpenOptions::new()
        .write(true)
        .create(fs_type.is_some())
        .truncate(fs_type.is_some())
        .open(&data_path)
        .unwrap();
    writer.seek(SeekFrom::Start(0)).unwrap();
    writer.write_all(&payload).unwrap();
    writer.sync_all().unwrap();
    drop(writer);
    let before = digest(&data_path);
    if fs_type.is_some() {
        assert_eq!(before, Sha256::digest(&payload).to_vec());
    } else {
        assert_eq!(before, digest(&disk.device));
    }
    let stats_request = csi::NodeGetVolumeStatsRequest {
        volume_id: name.into(),
        volume_path: target.to_string_lossy().into(),
        staging_target_path: stage.to_string_lossy().into(),
    };
    let before_stats = service
        .node_get_volume_stats(Request::new(stats_request.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(before_stats.usage[0].total > 0);
    drop(service);
    let service = NodeService::new(config).unwrap();
    service
        .node_stage_volume(Request::new(stage_request.clone()))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(publish_request.clone()))
        .await
        .unwrap();
    assert_eq!(
        digest(&data_path),
        before,
        "payload changed across service restart"
    );
    disk.grow(expanded);
    let expansion_path = if fs_type.is_some() {
        target.clone()
    } else {
        stage.clone()
    };
    let expansion = service
        .node_expand_volume(Request::new(csi::NodeExpandVolumeRequest {
            volume_id: name.into(),
            volume_path: expansion_path.to_string_lossy().into(),
            staging_target_path: stage.to_string_lossy().into(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: expanded as i64,
                limit_bytes: expanded as i64,
            }),
            volume_capability: Some(capability(fs_type)),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(expansion.capacity_bytes, expanded as i64);
    let after_stats = service
        .node_get_volume_stats(Request::new(stats_request))
        .await
        .unwrap()
        .into_inner();
    assert!(after_stats.usage[0].total > before_stats.usage[0].total);
    if fs_type.is_some() {
        assert_eq!(
            digest(&data_path),
            before,
            "filesystem growth changed payload"
        );
    } else {
        let mut reader = File::open(&target).unwrap();
        let mut observed = vec![0u8; payload.len()];
        reader.read_exact(&mut observed).unwrap();
        assert_eq!(observed, payload);
    }
    let unpublish = csi::NodeUnpublishVolumeRequest {
        volume_id: name.into(),
        target_path: target.to_string_lossy().into(),
    };
    let unstage = csi::NodeUnstageVolumeRequest {
        volume_id: name.into(),
        staging_target_path: stage.to_string_lossy().into(),
    };
    for _ in 0..2 {
        service
            .node_unpublish_volume(Request::new(unpublish.clone()))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        service
            .node_unstage_volume(Request::new(unstage.clone()))
            .await
            .unwrap();
    }
    assert!(!disk.mount_belongs_to_loop(&target));
    assert!(!disk.mount_belongs_to_loop(&stage));
    assert!(!target.exists(), "Unpublish must remove its target");
    assert!(stage.is_dir(), "the staging directory belongs to the CO");
    if fs_type.is_some() {
        // A mount capability may require RO even when attachment and Publish
        // readonly are false. The publication must retain the stronger stage mode.
        let mut readonly_stage = stage_request;
        if let Some(csi::volume_capability::AccessType::Mount(mount)) = readonly_stage
            .volume_capability
            .as_mut()
            .unwrap()
            .access_type
            .as_mut()
        {
            mount.mount_flags.push("ro".into());
        }
        let mut readonly_publish = publish_request;
        readonly_publish.volume_capability = readonly_stage.volume_capability.clone();
        service
            .node_stage_volume(Request::new(readonly_stage))
            .await
            .unwrap();
        service
            .node_publish_volume(Request::new(readonly_publish))
            .await
            .unwrap();
        assert!(OpenOptions::new().write(true).open(&data_path).is_err());
        assert_eq!(digest(&data_path), before);
        service
            .node_unpublish_volume(Request::new(unpublish))
            .await
            .unwrap();
        service
            .node_unstage_volume(Request::new(unstage))
            .await
            .unwrap();
        assert!(!target.exists());
        assert!(stage.is_dir());
    }
    eprintln!("{name}: real stage/publish, checksum, restart, stats, resize and cleanup passed");
}

async fn protected_readonly_block(root: &Path) {
    let root = root.join("readonly-block");
    std::fs::create_dir_all(&root).unwrap();
    let mut disk = LoopDisk::with_readonly(root.join("readonly.raw"), 8 * MIB, true);
    assert_eq!(
        run("blockdev", &["--getro".as_ref(), disk.device.as_os_str()]),
        "1"
    );
    let before = digest(&disk.device);
    let config = NodeConfig {
        node_id: "vm-linux-integration-test".into(),
        state_dir: root.join("state"),
        device_root: "/dev".into(),
        sys_block_dir: root.join("synthetic-sysfs-block"),
        kubelet_root: root.join("kubelet"),
    };
    fixture_serial(&config.sys_block_dir, &disk, "test-readonly-block");
    let stage = config.kubelet_root.join("plugins/test/globalmount");
    let target = config.kubelet_root.join("pods/test/volume/mount");
    disk.targets = vec![stage.clone(), target.clone()];
    let mut publish_context = context("test-readonly-block", None);
    publish_context.insert("readonly".into(), "true".into());
    let service = NodeService::new(config.clone()).unwrap();
    service
        .node_stage_volume(Request::new(csi::NodeStageVolumeRequest {
            volume_id: "readonly-block".into(),
            staging_target_path: stage.to_string_lossy().into(),
            publish_context: publish_context.clone(),
            volume_capability: Some(capability(None)),
            ..Default::default()
        }))
        .await
        .unwrap();
    let publish = csi::NodePublishVolumeRequest {
        volume_id: "readonly-block".into(),
        staging_target_path: stage.to_string_lossy().into(),
        target_path: target.to_string_lossy().into(),
        publish_context,
        volume_capability: Some(capability(None)),
        readonly: false,
        ..Default::default()
    };
    service
        .node_publish_volume(Request::new(publish.clone()))
        .await
        .unwrap();
    drop(service);
    let service = NodeService::new(config).unwrap();
    service
        .node_publish_volume(Request::new(publish))
        .await
        .unwrap();
    // Linux must deny an actual device write, not merely report a RO bind flag.
    if let Ok(mut writer) = OpenOptions::new().write(true).open(&target) {
        assert!(
            writer
                .write_all(b"must not reach the backing image")
                .is_err()
        );
    }
    assert_eq!(digest(&target), before);
    service
        .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
            volume_id: "readonly-block".into(),
            target_path: target.to_string_lossy().into(),
        }))
        .await
        .unwrap();
    service
        .node_unstage_volume(Request::new(csi::NodeUnstageVolumeRequest {
            volume_id: "readonly-block".into(),
            staging_target_path: stage.to_string_lossy().into(),
        }))
        .await
        .unwrap();
    assert!(!target.exists());
    assert!(stage.is_dir());
    assert_eq!(digest(&disk.device), before);
    eprintln!("raw block read-only: backend+Linux protection denies writes and survives restart");
}

async fn blank_adopted_volume_is_unchanged(root: &Path) {
    let root = root.join("blank-adopted");
    std::fs::create_dir_all(&root).unwrap();
    let disk = LoopDisk::new(root.join("blank.raw"), 64 * MIB);
    let original = digest(&disk.device);
    let config = NodeConfig {
        node_id: "vm-linux-integration-test".into(),
        state_dir: root.join("state"),
        device_root: "/dev".into(),
        sys_block_dir: root.join("synthetic-sysfs-block"),
        kubelet_root: root.join("kubelet"),
    };
    fixture_serial(&config.sys_block_dir, &disk, "test-blank");
    let staging_target_path = config
        .kubelet_root
        .join("plugins/test/globalmount")
        .to_string_lossy()
        .into();
    let service = NodeService::new(config).unwrap();
    let error = service
        .node_stage_volume(Request::new(csi::NodeStageVolumeRequest {
            volume_id: "blank-adopted".into(),
            staging_target_path,
            volume_capability: Some(capability(Some("xfs"))),
            publish_context: context("test-blank", Some("xfs")),
            volume_context: HashMap::from([("adopted".into(), "true".into())]),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        digest(&disk.device),
        original,
        "blank adopted device was modified"
    );
    eprintln!("blank adopted volume: refused without changing any byte");
}

// A private namespace may retain a filesystem after every mount visible to the
// driver is gone. The child touches only a mount and files created by this test.
struct PrivateMount {
    child: Child,
    finish: PathBuf,
}

impl PrivateMount {
    fn start(root: &Path, target: &Path) -> Self {
        let ready = root.join("namespace-ready");
        let write = root.join("namespace-write");
        let wrote = root.join("namespace-wrote");
        let finish = root.join("namespace-finish");
        let child = Command::new("unshare")
            .args(["--mount", "--propagation", "private", "sh", "-c"])
            .arg("touch \"$1\"; while [ ! -e \"$2\" ] && [ ! -e \"$5\" ]; do sleep 0.01; done; if [ -e \"$2\" ]; then printf private-copy > \"$4/still-live\" && sync -f \"$4/still-live\" && touch \"$3\"; fi; while [ ! -e \"$5\" ]; do sleep 0.01; done")
            .arg("owned-private-mount")
            .args([&ready, &write, &wrote, target, &finish])
            .spawn().unwrap();
        Self { child, finish }
    }

    fn finish(mut self) {
        File::create(&self.finish).unwrap();
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for PrivateMount {
    fn drop(&mut self) {
        // Killing this test child destroys its private mount namespace.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn test_config(root: &Path, disk: &LoopDisk, serial: &str) -> NodeConfig {
    let config = NodeConfig {
        node_id: "vm-linux-integration-test".into(),
        state_dir: root.join("state"),
        device_root: "/dev".into(),
        sys_block_dir: root.join("synthetic-sysfs-block"),
        kubelet_root: root.join("kubelet"),
    };
    fixture_serial(&config.sys_block_dir, disk, serial);
    config
}

async fn stage_ext4(service: &NodeService, serial: &str, target: &Path) {
    service
        .node_stage_volume(Request::new(csi::NodeStageVolumeRequest {
            volume_id: serial.into(),
            staging_target_path: target.to_str().unwrap().into(),
            volume_capability: Some(capability(Some("ext4"))),
            publish_context: context(serial, Some("ext4")),
            ..Default::default()
        }))
        .await
        .unwrap();
}

fn unstage_request(serial: &str, target: &Path) -> Request<csi::NodeUnstageVolumeRequest> {
    Request::new(csi::NodeUnstageVolumeRequest {
        volume_id: serial.into(),
        staging_target_path: target.to_str().unwrap().into(),
    })
}

async fn private_copy_prevents_successful_unstage(root: &Path) {
    let root = root.join("private-copy");
    std::fs::create_dir_all(&root).unwrap();
    let mut disk = LoopDisk::new(root.join("volume.raw"), 64 * MIB);
    run("mkfs.ext4", &["-q".as_ref(), disk.device.as_os_str()]);
    let config = test_config(&root, &disk, "test-private-copy");
    let serial_page = config
        .sys_block_dir
        .join(disk.device.file_name().unwrap())
        .join("device/vpd_pg80");
    let target = config.kubelet_root.join("plugins/test/globalmount");
    disk.targets.push(target.clone());
    let service = NodeService::new(config.clone()).unwrap();
    stage_ext4(&service, "test-private-copy", &target).await;
    let private = PrivateMount::start(&root, &target);
    wait_for_file(&root.join("namespace-ready")).await;
    let error = service
        .node_unstage_volume(unstage_request("test-private-copy", &target))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Aborted);
    assert!(
        !disk.mount_belongs_to_loop(&target),
        "driver namespace must already be unmounted"
    );
    File::create(root.join("namespace-write")).unwrap();
    wait_for_file(&root.join("namespace-wrote")).await;
    // A missing/unreadable serial is not proof of physical removal. The exact
    // recorded block device is still present and has a live private FS holder.
    std::fs::remove_file(serial_page).unwrap();
    // The previous error must retain durable intent, including across restart.
    drop(service);
    let service = NodeService::new(config).unwrap();
    assert_eq!(
        service
            .node_unstage_volume(unstage_request("test-private-copy", &target))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Aborted
    );
    private.finish();
    service
        .node_unstage_volume(unstage_request("test-private-copy", &target))
        .await
        .unwrap();
    eprintln!(
        "private mount copy: unstage remains ABORTED across restart and missing serial while copy can write and fsync; succeeds after copy exits"
    );
}

struct FrozenFilesystem {
    path: PathBuf,
    stop: Option<mpsc::Sender<()>>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    frozen: bool,
}

impl FrozenFilesystem {
    fn freeze(path: &Path) -> Self {
        run("fsfreeze", &["--freeze".as_ref(), path.as_os_str()]);
        let (send, receive) = mpsc::channel();
        let watched_path = path.to_owned();
        let watchdog = std::thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(15)).is_err() {
                let _ = Command::new("fsfreeze")
                    .arg("--unfreeze")
                    .arg(watched_path)
                    .status();
            }
        });
        Self {
            path: path.into(),
            stop: Some(send),
            watchdog: Some(watchdog),
            frozen: true,
        }
    }

    fn thaw(&mut self) {
        if self.frozen {
            // The watchdog may already have thawed it after a stalled assertion.
            let _ = Command::new("fsfreeze")
                .arg("--unfreeze")
                .arg(&self.path)
                .status();
            self.frozen = false;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

impl Drop for FrozenFilesystem {
    fn drop(&mut self) {
        self.thaw();
    }
}

async fn foreign_block_bind_on_device_filesystem_is_rejected(root: &Path) {
    let root = root.join("ext4-device-nodes");
    std::fs::create_dir_all(&root).unwrap();
    let mut carrier = LoopDisk::new(root.join("carrier.raw"), 64 * MIB);
    run("mkfs.ext4", &["-q".as_ref(), carrier.device.as_os_str()]);
    let carrier_mount = root.join("carrier");
    std::fs::create_dir(&carrier_mount).unwrap();
    carrier.targets.push(carrier_mount.clone());
    run(
        "mount",
        &[carrier.device.as_os_str(), carrier_mount.as_os_str()],
    );
    let mut disk = LoopDisk::new(root.join("block.raw"), 16 * MIB);
    let mut config = test_config(&root, &disk, "test-ext4-device-node");
    config.device_root = carrier_mount.join("nodes");
    std::fs::create_dir(&config.device_root).unwrap();
    let device_node = config.device_root.join(disk.device.file_name().unwrap());
    nix::sys::stat::mknod(
        &device_node,
        nix::sys::stat::SFlag::S_IFBLK,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        nix::sys::stat::makedev(disk.device_number.0, disk.device_number.1),
    )
    .unwrap();
    let foreign = root.join("foreign-bind");
    File::create(&foreign).unwrap();
    disk.targets.push(foreign.clone());
    run(
        "mount",
        &[
            "--bind".as_ref(),
            device_node.as_os_str(),
            foreign.as_os_str(),
        ],
    );
    let stage = config.kubelet_root.join("plugins/test/globalmount");
    let service = NodeService::new(config).unwrap();
    let request = csi::NodeStageVolumeRequest {
        volume_id: "test-ext4-device-node".into(),
        staging_target_path: stage.to_str().unwrap().into(),
        volume_capability: Some(capability(None)),
        publish_context: context("test-ext4-device-node", None),
        ..Default::default()
    };
    assert_eq!(
        service
            .node_stage_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        disk.mount_belongs_to_loop(&foreign),
        "foreign bind must remain untouched"
    );
    run("umount", &[foreign.as_os_str()]);
    service
        .node_stage_volume(Request::new(request))
        .await
        .unwrap();
    std::fs::remove_file(&device_node).unwrap();
    assert_eq!(
        service
            .node_unstage_volume(unstage_request("test-ext4-device-node", &stage))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable,
        "missing /dev inode must not imply hot-unplug while sysfs still contains the device"
    );
    nix::sys::stat::mknod(
        &device_node,
        nix::sys::stat::SFlag::S_IFBLK,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        nix::sys::stat::makedev(disk.device_number.0, disk.device_number.1),
    )
    .unwrap();
    service
        .node_unstage_volume(unstage_request("test-ext4-device-node", &stage))
        .await
        .unwrap();
    eprintln!(
        "custom ext4 device_root: foreign raw-block bind detected without scanning unrelated filesystem targets"
    );
    drop(disk);
    drop(carrier);
}

async fn shutdown_xfs_can_restart_and_clean_up(root: &Path) {
    let root = root.join("xfs-shutdown");
    std::fs::create_dir_all(&root).unwrap();
    let mut disk = LoopDisk::new(root.join("volume.raw"), 384 * MIB);
    run("mkfs.xfs", &["-f".as_ref(), disk.device.as_os_str()]);
    let config = test_config(&root, &disk, "test-xfs-shutdown");
    let stage = config.kubelet_root.join("plugins/test/globalmount");
    let target = config.kubelet_root.join("pods/test/volume/mount");
    disk.targets = vec![stage.clone(), target.clone()];
    let service = NodeService::new(config.clone()).unwrap();
    service
        .node_stage_volume(Request::new(csi::NodeStageVolumeRequest {
            volume_id: "test-xfs-shutdown".into(),
            staging_target_path: stage.to_str().unwrap().into(),
            volume_capability: Some(capability(Some("xfs"))),
            publish_context: context("test-xfs-shutdown", Some("xfs")),
            ..Default::default()
        }))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(csi::NodePublishVolumeRequest {
            volume_id: "test-xfs-shutdown".into(),
            staging_target_path: stage.to_str().unwrap().into(),
            target_path: target.to_str().unwrap().into(),
            volume_capability: Some(capability(Some("xfs"))),
            publish_context: context("test-xfs-shutdown", Some("xfs")),
            ..Default::default()
        }))
        .await
        .unwrap();
    let data = target.join("before-shutdown");
    let mut file = File::create(&data).unwrap();
    file.write_all(b"owned XFS shutdown test").unwrap();
    file.sync_all().unwrap();
    drop(file);
    // XFS_IOC_GOINGDOWN targets only the fresh filesystem owned by this test.
    run(
        "xfs_io",
        &[
            "-x".as_ref(),
            "-c".as_ref(),
            "shutdown".as_ref(),
            target.as_os_str(),
        ],
    );
    assert_eq!(
        std::fs::metadata(&data).unwrap_err().raw_os_error(),
        Some(nix::libc::EIO)
    );
    drop(service);
    let service = NodeService::new(config).expect("shutdown XFS must not prevent driver restart");
    service
        .node_unpublish_volume(Request::new(csi::NodeUnpublishVolumeRequest {
            volume_id: "test-xfs-shutdown".into(),
            target_path: target.to_str().unwrap().into(),
        }))
        .await
        .unwrap();
    service
        .node_unstage_volume(unstage_request("test-xfs-shutdown", &stage))
        .await
        .unwrap();
    assert!(!disk.mount_belongs_to_loop(&stage));
    assert!(!target.exists());
    eprintln!(
        "XFS shutdown: real EIO from getattr, driver restart, unpublish and unstage cleanup passed"
    );
}

fn assert_umount_inherits_process_lock(target: &Path, lock: &Path) {
    let lock = std::fs::metadata(lock).unwrap();
    let target = target.as_os_str().as_encoded_bytes();
    let mut found = false;
    for process in std::fs::read_dir("/proc").unwrap().flatten() {
        if process
            .file_name()
            .to_string_lossy()
            .parse::<u32>()
            .is_err()
        {
            continue;
        }
        let Ok(command) = std::fs::read(process.path().join("cmdline")) else {
            continue;
        };
        let fields: Vec<_> = command.split(|byte| *byte == 0).collect();
        if fields.first() != Some(&b"umount".as_slice()) || !fields.contains(&target) {
            continue;
        }
        let inherited = std::fs::read_dir(process.path().join("fd"))
            .unwrap()
            .flatten()
            .any(|fd| {
                std::fs::metadata(fd.path()).is_ok_and(|metadata| {
                    metadata.dev() == lock.dev() && metadata.ino() == lock.ino()
                })
            });
        assert!(
            inherited,
            "pending real umount must retain the driver's process lock fd"
        );
        found = true;
    }
    assert!(found, "expected this test's pending umount subprocess");
}

async fn cancelled_unstage_waits_for_kernel_completion(root: &Path) {
    let root = root.join("cancelled-unmount");
    std::fs::create_dir_all(&root).unwrap();
    // Only this test-owned outer filesystem is frozen; the driver registry and
    // ownership markers remain on the ordinary scratch filesystem.
    let mut outer = LoopDisk::new(root.join("backing.raw"), 192 * MIB);
    run("mkfs.ext4", &["-q".as_ref(), outer.device.as_os_str()]);
    let backing_mount = root.join("backing");
    std::fs::create_dir(&backing_mount).unwrap();
    outer.targets.push(backing_mount.clone());
    run(
        "mount",
        &[outer.device.as_os_str(), backing_mount.as_os_str()],
    );
    let mut disk = LoopDisk::with_marker(
        backing_mount.join("volume.raw"),
        64 * MIB,
        false,
        Some(root.join("inner.loop-owner")),
    );
    run("mkfs.ext4", &["-q".as_ref(), disk.device.as_os_str()]);
    let config = test_config(&root, &disk, "test-delayed-unmount");
    let target = config.kubelet_root.join("plugins/test/globalmount");
    disk.targets.push(target.clone());
    let process_lock = config.state_dir.join("node.lock");
    let service = Arc::new(NodeService::new(config).unwrap());
    stage_ext4(&service, "test-delayed-unmount", &target).await;
    File::create(target.join("dirty"))
        .unwrap()
        .write_all(&vec![b'x'; 2 * MIB as usize])
        .unwrap();
    let mut frozen = FrozenFilesystem::freeze(&backing_mount);
    let request = unstage_request("test-delayed-unmount", &target);
    let worker_service = service.clone();
    let waiter = tokio::spawn(async move { worker_service.node_unstage_volume(request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while disk.mount_belongs_to_loop(&target) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !waiter.is_finished(),
        "umount must still wait for the frozen backing filesystem"
    );
    assert_umount_inherits_process_lock(&target, &process_lock);
    waiter.abort();
    let _ = waiter.await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        service.node_unstage_volume(unstage_request("test-delayed-unmount", &target)),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        error.code(),
        tonic::Code::Aborted,
        "RPC cancellation must not release the in-flight volume guard"
    );
    frozen.thaw();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match service
            .node_unstage_volume(unstage_request("test-delayed-unmount", &target))
            .await
        {
            Ok(_) => break,
            Err(error) if error.code() == tonic::Code::Aborted && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(error) => panic!("retry after kernel completion: {error}"),
        }
    }
    eprintln!(
        "cancelled unstage: mountinfo absent while kernel flush blocks; concurrent retry ABORTED until actual completion"
    );
    // Drop inner loop before unmounting the outer filesystem that backs it.
    drop(disk);
    drop(outer);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicitly opted-in disposable privileged Linux container and storage utilities"]
async fn real_linux_filesystem_and_block_lifecycle() {
    assert_eq!(
        std::env::var("CLEVER_CSI_PRIVILEGED_TESTS").as_deref(),
        Ok("1"),
        "set CLEVER_CSI_PRIVILEGED_TESTS=1 only in a disposable privileged container"
    );
    let base = PathBuf::from(
        std::env::var_os("TMPDIR")
            .unwrap_or_else(|| OsString::from("/var/tmp/clever-csi-linux-tests")),
    );
    assert!(
        base.is_absolute() && !base.starts_with("/tmp"),
        "TMPDIR must be an absolute directory outside /tmp"
    );
    std::fs::create_dir_all(&base).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("clever-csi-owned-loops-")
        .tempdir_in(base)
        .unwrap()
        .keep();
    // Keep scratch artifacts on panic so the outer cleanup can still read
    // ownership markers if a destructor could not detach a loop.
    lifecycle(&directory, Some("xfs")).await;
    lifecycle(&directory, Some("ext4")).await;
    lifecycle(&directory, None).await;
    protected_readonly_block(&directory).await;
    blank_adopted_volume_is_unchanged(&directory).await;
    private_copy_prevents_successful_unstage(&directory).await;
    cancelled_unstage_waits_for_kernel_completion(&directory).await;
    shutdown_xfs_can_restart_and_clean_up(&directory).await;
    foreign_block_bind_on_device_filesystem_is_rejected(&directory).await;
    let mut pending = vec![directory.clone()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            let kind = entry.file_type().unwrap();
            assert!(
                entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "loop-owner"),
                "loop cleanup incomplete: preserve ownership markers for outer recovery"
            );
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}
