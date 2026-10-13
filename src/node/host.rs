use std::{
    collections::BTreeSet,
    ffi::OsString,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tonic::Status;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Device {
    pub path: PathBuf,
    pub major: u64,
    pub minor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mount {
    pub target: PathBuf,
    pub root: PathBuf,
    pub major: u64,
    pub minor: u64,
    pub fs_type: String,
    pub options: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Usage {
    pub available: i64,
    pub total: i64,
    pub used: i64,
    pub inodes_available: i64,
    pub inodes_total: i64,
    pub inodes_used: i64,
}

#[async_trait]
pub(super) trait Host: Send + Sync {
    async fn discover(&self, serial: &str) -> Result<Device, Status>;
    async fn filesystem(&self, device: &Device) -> Result<Option<String>, Status>;
    async fn mounts(&self) -> Result<Vec<Mount>, Status>;
    async fn mount_fs(
        &self,
        device: &Device,
        target: &Path,
        fs_type: &str,
        flags: &[String],
    ) -> Result<(), Status>;
    async fn bind(&self, source: &Path, target: &Path, flags: &[String]) -> Result<(), Status>;
    async fn remount_bind(&self, target: &Path, flags: &[String]) -> Result<(), Status>;
    async fn unmount(&self, target: &Path) -> Result<(), Status>;
    async fn grow(&self, device: &Device, target: &Path, fs_type: &str) -> Result<(), Status>;
    async fn capacity(&self, device: &Device) -> Result<i64, Status>;
    async fn readonly(&self, device: &Device) -> Result<bool, Status>;
    async fn rescan(&self, device: &Device, serial: &str) -> Result<(), Status>;
    fn block_identity(&self, target: &Path) -> Result<Option<(u64, u64)>, Status>;
    fn usage(&self, target: &Path) -> Result<Usage, Status>;
}

pub(super) struct LinuxHost {
    pub device_root: PathBuf,
    pub sys_block_dir: PathBuf,
}

/// SPC unit serial number VPD page, as exposed by Linux for QEMU scsi-hd.
/// The two-byte length is big endian; accepting truncation or a different page
/// would turn an unrelated identifier into an apparent serial match.
fn vpd_serial(bytes: &[u8]) -> Result<String, Status> {
    if bytes.len() < 4 || bytes[1] != 0x80 || bytes[0] & 0x1f != 0 {
        return Err(Status::failed_precondition(
            "invalid SCSI unit serial VPD page",
        ));
    }
    let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    if length == 0 || length > 256 || bytes.len() != length + 4 {
        return Err(Status::failed_precondition(
            "invalid SCSI unit serial length",
        ));
    }
    let serial = std::str::from_utf8(&bytes[4..])
        .map_err(|_| Status::failed_precondition("SCSI unit serial is not ASCII"))?;
    // libata pads the fixed-width ATA serial with spaces in page 0x80.
    // Strip transport padding only; never truncate or rewrite the identifier.
    let serial = serial.trim_matches(' ');
    if serial.is_empty() || !serial.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Status::failed_precondition(
            "SCSI unit serial contains non-printable bytes or whitespace",
        ));
    }
    Ok(serial.to_owned())
}

async fn matching_serial(root: &Path, serial: &str) -> Result<bool, Status> {
    let mut values = Vec::new();
    let mut invalid = false;
    for name in ["serial", "device/serial"] {
        match tokio::fs::read_to_string(root.join(name)).await {
            Ok(value) => values.push(value.trim_matches([' ', '\n', '\r']).to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => invalid = true,
        }
    }
    match tokio::fs::read(root.join("device/vpd_pg80")).await {
        Ok(bytes) => match vpd_serial(&bytes) {
            Ok(value) => values.push(value),
            Err(_) => invalid = true,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => invalid = true,
    }
    if !values.iter().any(|value| value == serial) {
        // An unrelated disk (or an entry disappearing during hotplug) must not
        // prevent finding this volume. A matching candidate still fails closed.
        return Ok(false);
    }
    if invalid || values.iter().any(|value| value != serial) {
        return Err(Status::failed_precondition(
            "conflicting or unreadable serial attributes for the matching block device",
        ));
    }
    Ok(true)
}

fn io_status(action: &str, error: impl std::fmt::Display) -> Status {
    Status::internal(format!("{action}: {error}"))
}

async fn command(program: &str, args: &[OsString]) -> Result<std::process::Output, Status> {
    let mut child = tokio::process::Command::new(program);
    child.args(args).kill_on_drop(true).env("LC_ALL", "C");
    tokio::time::timeout(Duration::from_secs(120), child.output())
        .await
        .map_err(|_| Status::deadline_exceeded(format!("{program} timed out")))?
        .map_err(|e| io_status(program, e))
}

async fn checked(program: &str, args: &[OsString]) -> Result<(), Status> {
    let output = command(program, args).await?;
    if !output.status.success() {
        // Do not leak arbitrary subprocess output through the CSI socket.
        return Err(Status::internal(format!(
            "{program} failed ({})",
            output.status
        )));
    }
    Ok(())
}

fn decode_mount_field(value: &str) -> Result<PathBuf, Status> {
    let input = value.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'\\' {
            let code = input
                .get(index + 1..index + 4)
                .ok_or_else(|| Status::internal("invalid mountinfo escape"))?;
            let value = match code {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(Status::internal("invalid mountinfo escape")),
            };
            bytes.push(value);
            index += 4;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

pub(super) fn parse_mountinfo(text: &str) -> Result<Vec<Mount>, Status> {
    text.lines()
        .map(|line| {
            let (left, right) = line
                .split_once(" - ")
                .ok_or_else(|| Status::internal("invalid mountinfo separator"))?;
            let fields: Vec<_> = left.split_whitespace().collect();
            let tail: Vec<_> = right.split_whitespace().collect();
            if fields.len() < 6 || tail.len() < 3 {
                return Err(Status::internal("invalid mountinfo fields"));
            }
            let (major, minor) = fields[2]
                .split_once(':')
                .ok_or_else(|| Status::internal("invalid mountinfo device"))?;
            Ok(Mount {
                target: decode_mount_field(fields[4])?,
                root: decode_mount_field(fields[3])?,
                major: major
                    .parse()
                    .map_err(|_| Status::internal("invalid mountinfo major"))?,
                minor: minor
                    .parse()
                    .map_err(|_| Status::internal("invalid mountinfo minor"))?,
                fs_type: tail[0].to_owned(),
                options: fields[5].split(',').map(str::to_owned).collect(),
            })
        })
        .collect()
}

#[async_trait]
impl Host for LinuxHost {
    async fn discover(&self, serial: &str) -> Result<Device, Status> {
        let mut entries = tokio::fs::read_dir(&self.sys_block_dir)
            .await
            .map_err(|e| io_status("read sysfs block devices", e))?;
        let mut matches = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| io_status("read sysfs device", e))?
        {
            let root = entry.path();
            if !matching_serial(&root, serial).await? {
                continue;
            }
            let path = self.device_root.join(entry.file_name());
            let canonical = tokio::fs::canonicalize(&path)
                .await
                .map_err(|e| io_status("resolve device", e))?;
            if !canonical.starts_with(&self.device_root) {
                return Err(Status::failed_precondition(
                    "device resolves outside device_root",
                ));
            }
            let metadata = tokio::fs::metadata(&canonical)
                .await
                .map_err(|e| io_status("stat device", e))?;
            if !metadata.file_type().is_block_device() {
                return Err(Status::failed_precondition(
                    "serial matched a non-block device",
                ));
            }
            let major = nix::sys::stat::major(metadata.rdev());
            let minor = nix::sys::stat::minor(metadata.rdev());
            let sys_identity = tokio::fs::read_to_string(root.join("dev"))
                .await
                .map_err(|e| io_status("read sysfs identity", e))?;
            if sys_identity.trim() != format!("{major}:{minor}") {
                return Err(Status::failed_precondition(
                    "device node differs from sysfs identity",
                ));
            }
            matches.push(Device {
                path: canonical,
                major,
                minor,
            });
        }
        match matches.len() {
            0 => Err(Status::unavailable(
                "volume device serial is not present; retry after attachment",
            )),
            1 => Ok(matches.remove(0)),
            _ => Err(Status::failed_precondition("device serial is ambiguous")),
        }
    }

    async fn filesystem(&self, device: &Device) -> Result<Option<String>, Status> {
        let output = command(
            "blkid",
            &[
                "-p".into(),
                "-s".into(),
                "TYPE".into(),
                "-o".into(),
                "value".into(),
                "--".into(),
                device.path.as_os_str().into(),
            ],
        )
        .await?;
        if output.status.code() == Some(2) {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(Status::failed_precondition(
                "blkid could not identify an unambiguous filesystem",
            ));
        }
        let fs = String::from_utf8(output.stdout)
            .map_err(|_| Status::failed_precondition("invalid filesystem signature"))?;
        let fs = fs.trim();
        if fs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(fs.to_owned()))
        }
    }

    async fn mounts(&self) -> Result<Vec<Mount>, Status> {
        let text = tokio::fs::read_to_string("/proc/self/mountinfo")
            .await
            .map_err(|e| io_status("read mountinfo", e))?;
        parse_mountinfo(&text)
    }

    async fn mount_fs(
        &self,
        device: &Device,
        target: &Path,
        fs_type: &str,
        flags: &[String],
    ) -> Result<(), Status> {
        checked(
            "mount",
            &[
                "-t".into(),
                fs_type.into(),
                "-o".into(),
                flags.join(",").into(),
                "--".into(),
                device.path.as_os_str().into(),
                target.as_os_str().into(),
            ],
        )
        .await
    }

    async fn bind(&self, source: &Path, target: &Path, flags: &[String]) -> Result<(), Status> {
        checked(
            "mount",
            &[
                "--bind".into(),
                "--".into(),
                source.as_os_str().into(),
                target.as_os_str().into(),
            ],
        )
        .await?;
        self.remount_bind(target, flags).await
    }

    async fn remount_bind(&self, target: &Path, flags: &[String]) -> Result<(), Status> {
        // Filesystem/superblock options were set on the staging mount. Only
        // per-mount VFS flags belong on the bind remount.
        let flags: Vec<_> = flags
            .iter()
            .filter(|flag| !["nouuid", "discard", "nodiscard"].contains(&flag.as_str()))
            .map(String::as_str)
            .collect();
        let options = format!("remount,bind,{}", flags.join(","));
        checked(
            "mount",
            &[
                "-o".into(),
                options.into(),
                "--".into(),
                target.as_os_str().into(),
            ],
        )
        .await
    }

    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        // Never lazy/force unmount: success must mean the mount has gone away.
        checked("umount", &["--".into(), target.as_os_str().into()]).await
    }

    async fn grow(&self, device: &Device, target: &Path, fs_type: &str) -> Result<(), Status> {
        match fs_type {
            "xfs" => checked("xfs_growfs", &[target.as_os_str().into()]).await,
            "ext4" => checked("resize2fs", &[device.path.as_os_str().into()]).await,
            _ => Err(Status::failed_precondition("unsupported filesystem")),
        }
    }

    async fn capacity(&self, device: &Device) -> Result<i64, Status> {
        let output = command(
            "blockdev",
            &["--getsize64".into(), device.path.as_os_str().into()],
        )
        .await?;
        if !output.status.success() {
            return Err(Status::internal("blockdev failed"));
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<i64>()
            .map_err(|_| Status::internal("invalid block device size"))
    }

    async fn readonly(&self, device: &Device) -> Result<bool, Status> {
        let output = command(
            "blockdev",
            &["--getro".into(), device.path.as_os_str().into()],
        )
        .await?;
        if !output.status.success() {
            return Err(Status::failed_precondition(
                "cannot verify block device read-only protection",
            ));
        }
        match output.stdout.as_slice() {
            b"1\n" => Ok(true),
            b"0\n" => Ok(false),
            _ => Err(Status::failed_precondition(
                "invalid block device read-only status",
            )),
        }
    }

    async fn rescan(&self, device: &Device, serial: &str) -> Result<(), Status> {
        // Never derive a sysfs write path from caller-supplied context. Recheck
        // the serial and dev_t immediately before writing to the discovered disk.
        if self.discover(serial).await? != *device {
            return Err(Status::failed_precondition(
                "device identity changed before rescan",
            ));
        }
        let name = device
            .path
            .file_name()
            .ok_or_else(|| Status::failed_precondition("device has no sysfs name"))?;
        let root = self.sys_block_dir.join(name);
        let identity = tokio::fs::read_to_string(root.join("dev"))
            .await
            .map_err(|error| io_status("read rescan device identity", error))?;
        if identity.trim() != format!("{}:{}", device.major, device.minor) {
            return Err(Status::failed_precondition(
                "sysfs device identity changed before rescan",
            ));
        }
        match tokio::fs::write(root.join("device/rescan"), b"1\n").await {
            Ok(()) => (),
            // virtio-blk reports resize asynchronously and has no SCSI rescan.
            // A subsequent RPC retries the capacity check without guessing an API.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_status("rescan block device", error)),
        }
        if self.discover(serial).await? != *device {
            return Err(Status::failed_precondition(
                "device identity changed during rescan",
            ));
        }
        Ok(())
    }

    fn block_identity(&self, target: &Path) -> Result<Option<(u64, u64)>, Status> {
        let metadata = match std::fs::metadata(target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_status("stat mount target", error)),
        };
        if !metadata.file_type().is_block_device() {
            return Ok(None);
        }
        Ok(Some((
            nix::sys::stat::major(metadata.rdev()),
            nix::sys::stat::minor(metadata.rdev()),
        )))
    }

    fn usage(&self, target: &Path) -> Result<Usage, Status> {
        let stat = nix::sys::statvfs::statvfs(target).map_err(|e| io_status("statvfs", e))?;
        let bounded = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        Ok(Usage {
            available: bounded(stat.blocks_available().saturating_mul(stat.fragment_size())),
            total: bounded(stat.blocks().saturating_mul(stat.fragment_size())),
            used: bounded(
                stat.blocks()
                    .saturating_sub(stat.blocks_free())
                    .saturating_mul(stat.fragment_size()),
            ),
            inodes_available: bounded(stat.files_available()),
            inodes_total: bounded(stat.files()),
            inodes_used: bounded(stat.files().saturating_sub(stat.files_free())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_handles_optional_fields_and_escaped_paths() {
        let result = parse_mountinfo("36 25 8:17 / /var/lib/kubelet/a\\040b rw,nosuid,nodev shared:7 master:2 - xfs /dev/vdb rw,attr2\n").unwrap();
        assert_eq!(result[0].target, Path::new("/var/lib/kubelet/a b"));
        assert_eq!((result[0].major, result[0].minor), (8, 17));
        assert!(result[0].options.contains("nodev"));
    }

    #[test]
    fn malformed_mountinfo_fails_closed() {
        assert!(parse_mountinfo("not mountinfo").is_err());
        assert!(parse_mountinfo("36 25 8:1 / /bad\\999 rw - xfs /dev/x rw").is_err());
    }

    #[test]
    fn qemu_scsi_unit_serial_decodes_exactly() {
        let serial = b"clever-volume-abc123";
        let mut page = vec![0, 0x80, 0, serial.len() as u8];
        page.extend_from_slice(serial);
        assert_eq!(vpd_serial(&page).unwrap(), "clever-volume-abc123");
        let long = vec![b'x'; 256];
        let mut page = vec![0, 0x80, 1, 0];
        page.extend(long);
        assert_eq!(vpd_serial(&page).unwrap().len(), 256);
        let mut ata_page = vec![0, 0x80, 0, 20];
        ata_page.extend_from_slice(b"  ATA-device-42");
        ata_page.resize(24, b' ');
        assert_eq!(vpd_serial(&ata_page).unwrap(), "ATA-device-42");
    }

    #[test]
    fn malformed_vpd_serials_are_not_identifiers() {
        for bytes in [
            vec![],
            vec![0, 0x83, 0, 1, b'x'],
            vec![0, 0x80, 0, 2, b'x'],
            vec![0, 0x80, 0, 1, b'x', b'y'],
            vec![0, 0x80, 0, 1, 0],
            vec![0, 0x80, 0, 1, b' '],
            vec![0, 0x80, 0, 1, 255],
        ] {
            assert!(vpd_serial(&bytes).is_err());
        }
    }

    #[tokio::test]
    async fn unrelated_bad_serial_pages_are_isolated_but_matching_conflicts_fail_closed() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/node-tests");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        std::fs::create_dir(root.path().join("device")).unwrap();
        let vpd = root.path().join("device/vpd_pg80");
        std::fs::write(&vpd, [0, 0x80, 0, 20, b'x']).unwrap();
        assert!(!matching_serial(root.path(), "wanted").await.unwrap());
        std::fs::write(root.path().join("serial"), b"wanted\n").unwrap();
        assert_eq!(
            matching_serial(root.path(), "wanted")
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        std::fs::write(&vpd, [0, 0x80, 0, 5, b'o', b't', b'h', b'e', b'r']).unwrap();
        assert!(matching_serial(root.path(), "wanted").await.is_err());
        let mut page = vec![0, 0x80, 0, 8];
        page.extend_from_slice(b" wanted ");
        std::fs::write(&vpd, page).unwrap();
        assert!(matching_serial(root.path(), "wanted").await.unwrap());
        assert!(!matching_serial(root.path(), "wante").await.unwrap());
    }
}
