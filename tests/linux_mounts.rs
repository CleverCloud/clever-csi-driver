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
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
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
}

impl LoopDisk {
    fn new(backing: PathBuf, bytes: u64) -> Self {
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
        let device = PathBuf::from(run(
            "losetup",
            &["--find".as_ref(), "--show".as_ref(), backing.as_os_str()],
        ));
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
        let ownership_marker = backing.with_extension("loop-owner");
        let mut marker = File::create(&ownership_marker).unwrap();
        writeln!(marker, "{}\n{}", device.display(), backing.display()).unwrap();
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
            .args(["--noheadings", "--raw", "--output", "BACK-FILE"])
            .arg(&self.device)
            .output();
        let expected =
            std::fs::canonicalize(&self.backing).unwrap_or_else(|_| self.backing.clone());
        let owned = ownership.is_ok_and(|output| {
            output.status.success()
                && Path::new(String::from_utf8_lossy(&output.stdout).trim()) == expected
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
    std::fs::create_dir_all(entry.join("device")).unwrap();
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
    let config = NodeConfig {
        node_id: "vm-linux-integration-test".into(),
        state_dir: root.join("state"),
        device_root: "/dev".into(),
        sys_block_dir: root.join("synthetic-sysfs-block"),
        kubelet_root: root.join("kubelet"),
    };
    fixture_serial(&config.sys_block_dir, &disk, &format!("test-{name}"));
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
    service
        .node_stage_volume(Request::new(stage_request.clone()))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(publish_request.clone()))
        .await
        .unwrap();
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
        .node_stage_volume(Request::new(stage_request))
        .await
        .unwrap();
    service
        .node_publish_volume(Request::new(publish_request))
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
    eprintln!("{name}: real stage/publish, checksum, restart, stats, resize and cleanup passed");
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
    blank_adopted_volume_is_unchanged(&directory).await;
    std::fs::remove_dir_all(directory).unwrap();
}
