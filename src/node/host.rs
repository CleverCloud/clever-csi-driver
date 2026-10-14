use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
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
    async fn ensure_released(&self, device: &Device) -> Result<(), Status>;
    async fn device_filesystem(&self, device: &Device) -> Result<Option<(u64, u64)>, Status>;
    async fn block_identity(&self, target: &Path) -> Result<Option<(u64, u64)>, Status>;
    async fn usage(&self, target: &Path) -> Result<Usage, Status>;
}

pub(super) struct LinuxHost {
    pub device_root: PathBuf,
    pub sys_block_dir: PathBuf,
    // Acquired with flock and released only by closing the last shared fd.
    // An explicit LOCK_UN (including nix::Flock::drop) defeats child inheritance.
    pub process_lock: Arc<File>,
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

fn discovery_io_status(action: &str, error: std::io::Error) -> Status {
    if matches!(
        error.raw_os_error(),
        Some(nix::libc::ENOENT | nix::libc::ENXIO | nix::libc::ENODEV)
    ) {
        Status::unavailable(
            "matching block device is not yet accessible; retry after device discovery",
        )
    } else {
        io_status(action, error)
    }
}

impl LinuxHost {
    async fn command(
        &self,
        program: &str,
        args: &[OsString],
    ) -> Result<std::process::Output, Status> {
        let mut child = tokio::process::Command::new(program);
        child.args(args).env("LC_ALL", "C");
        let lock_fd = self.process_lock.as_raw_fd();
        // SAFETY: pre_exec executes only async-signal-safe fcntl calls, performs
        // no allocation, and changes flags only in the forked child's fd table.
        // The host retains the Arc<File> until the command has been reaped.
        unsafe {
            child.pre_exec(move || {
                let flags = nix::libc::fcntl(lock_fd, nix::libc::F_GETFD);
                if flags == -1
                    || nix::libc::fcntl(lock_fd, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC)
                        == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The caller runs in an RPC-independent worker. Neither a timeout nor
        // SIGKILL proves a mount/umount syscall has completed. Await the actual
        // exit, while the child also prevents a new process taking node.lock.
        child.output().await.map_err(|e| io_status(program, e))
    }

    async fn checked(&self, program: &str, args: &[OsString]) -> Result<(), Status> {
        let output = self.command(program, args).await?;
        if !output.status.success() {
            // Do not leak arbitrary subprocess output through the CSI socket.
            return Err(Status::internal(format!(
                "{program} failed ({})",
                output.status
            )));
        }
        Ok(())
    }
}

fn decode_mount_field(input: &[u8]) -> Result<PathBuf, Status> {
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

fn ascii_field(value: &[u8]) -> Result<&str, Status> {
    std::str::from_utf8(value).map_err(|_| Status::internal("non-ASCII mountinfo metadata"))
}

pub(super) fn parse_mountinfo(bytes: &[u8]) -> Result<Vec<Mount>, Status> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let separator = line
                .windows(3)
                .position(|part| part == b" - ")
                .ok_or_else(|| Status::internal("invalid mountinfo separator"))?;
            let fields: Vec<_> = line[..separator].split(|byte| *byte == b' ').collect();
            // Keep the empty source field. Linux permits e.g. `- tmpfs  rw`.
            // Paths are kernel-escaped bytes, not Unicode strings.
            let tail: Vec<_> = line[separator + 3..].split(|byte| *byte == b' ').collect();
            if fields.len() < 6
                || fields[..6].iter().any(|field| field.is_empty())
                || tail.len() != 3
                || tail[0].is_empty()
                || tail[2].is_empty()
            {
                return Err(Status::internal("invalid mountinfo fields"));
            }
            let (major, minor) = ascii_field(fields[2])?
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
                fs_type: ascii_field(tail[0])?.to_owned(),
                options: ascii_field(fields[5])?
                    .split(',')
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}

/// Verify the actual Linux bus/driver ancestry rather than guessing transport
/// from /dev names. Serial collisions on boot NVMe, SATA or virtio-blk disks do
/// not make those disks valid Axo virtio-scsi attachments.
async fn require_virtio_scsi(root: &Path) -> Result<(), Status> {
    let device = tokio::fs::canonicalize(root.join("device"))
        .await
        .map_err(|_| Status::failed_precondition("matching device has no SCSI topology"))?;
    let subsystem = tokio::fs::canonicalize(device.join("subsystem")).await;
    let kind = tokio::fs::read_to_string(device.join("type")).await;
    if !subsystem.is_ok_and(|path| path.file_name().is_some_and(|name| name == "scsi"))
        || !kind.is_ok_and(|kind| kind.trim() == "0")
    {
        return Err(Status::failed_precondition(
            "matching device is not a SCSI disk",
        ));
    }
    for ancestor in device.ancestors().skip(1) {
        let driver = tokio::fs::canonicalize(ancestor.join("driver")).await;
        if driver.is_ok_and(|path| path.file_name().is_some_and(|name| name == "virtio_scsi")) {
            let bus = tokio::fs::canonicalize(ancestor.join("subsystem")).await;
            if bus.is_ok_and(|path| path.file_name().is_some_and(|name| name == "virtio")) {
                return Ok(());
            }
        }
    }
    Err(Status::failed_precondition(
        "matching SCSI disk is not on virtio-scsi",
    ))
}

/// A missing /dev path can be a udev rename, not a physical removal. Only the
/// absence of the recorded dev_t from a readable sysfs block table proves that
/// there is no current kernel device to check. Disappearing unrelated entries
/// are harmless; every other read/parse failure leaves the result unknown.
fn device_present_in_sysfs(root: &Path, device: &Device) -> Result<bool, Status> {
    let unknown = || Status::unavailable("cannot establish whether the recorded device is gone");
    let entries = std::fs::read_dir(root).map_err(|_| unknown())?;
    for entry in entries {
        let entry = entry.map_err(|_| unknown())?;
        let text = match std::fs::read_to_string(entry.path().join("dev")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(unknown()),
        };
        let (major, minor) = text.trim().split_once(':').ok_or_else(unknown)?;
        let major = major.parse::<u64>().map_err(|_| unknown())?;
        let minor = minor.parse::<u64>().map_err(|_| unknown())?;
        if (major, minor) == (device.major, device.minor) {
            return Ok(true);
        }
    }
    Ok(false)
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
            require_virtio_scsi(&root).await?;
            let path = self.device_root.join(entry.file_name());
            let canonical = tokio::fs::canonicalize(&path)
                .await
                .map_err(|e| discovery_io_status("resolve device", e))?;
            if !canonical.starts_with(&self.device_root) {
                return Err(Status::failed_precondition(
                    "device resolves outside device_root",
                ));
            }
            let metadata = tokio::fs::metadata(&canonical)
                .await
                .map_err(|e| discovery_io_status("stat device", e))?;
            if !metadata.file_type().is_block_device() {
                return Err(Status::failed_precondition(
                    "serial matched a non-block device",
                ));
            }
            let major = nix::sys::stat::major(metadata.rdev());
            let minor = nix::sys::stat::minor(metadata.rdev());
            let sys_identity = tokio::fs::read_to_string(root.join("dev"))
                .await
                .map_err(|e| discovery_io_status("read sysfs identity", e))?;
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
        let output = self
            .command(
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
        let text = tokio::fs::read("/proc/self/mountinfo")
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
        self.checked(
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
        self.checked(
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
        self.checked(
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
        self.checked("umount", &["--".into(), target.as_os_str().into()])
            .await
    }

    async fn grow(&self, device: &Device, target: &Path, fs_type: &str) -> Result<(), Status> {
        match fs_type {
            "xfs" => {
                self.checked("xfs_growfs", &[target.as_os_str().into()])
                    .await
            }
            "ext4" => {
                self.checked("resize2fs", &[device.path.as_os_str().into()])
                    .await
            }
            _ => Err(Status::failed_precondition("unsupported filesystem")),
        }
    }

    async fn capacity(&self, device: &Device) -> Result<i64, Status> {
        let output = self
            .command(
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
        let output = self
            .command(
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
            Err(error) => return Err(io_status("rescan block device", error)),
        }
        if self.discover(serial).await? != *device {
            return Err(Status::failed_precondition(
                "device identity changed during rescan",
            ));
        }
        Ok(())
    }

    async fn ensure_released(&self, device: &Device) -> Result<(), Status> {
        let device = device.clone();
        let sys_block_dir = self.sys_block_dir.clone();
        tokio::task::spawn_blocking(move || {
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_EXCL | nix::libc::O_NOFOLLOW)
                .open(&device.path)
            {
                Ok(file) => file,
                Err(error) if error.raw_os_error() == Some(nix::libc::EBUSY) => {
                    return Err(Status::aborted(
                        "volume device is still held by a filesystem or kernel user",
                    ));
                }
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(nix::libc::ENOENT | nix::libc::ENXIO | nix::libc::ENODEV)
                    ) =>
                {
                    if device_present_in_sysfs(&sys_block_dir, &device)? {
                        return Err(Status::unavailable(
                            "recorded device remains present in sysfs but cannot be opened",
                        ));
                    }
                    return Ok(());
                }
                Err(error) => return Err(io_status("open volume device exclusively", error)),
            };
            let metadata = file
                .metadata()
                .map_err(|error| io_status("inspect exclusive device", error))?;
            if !metadata.file_type().is_block_device()
                || (
                    nix::sys::stat::major(metadata.rdev()),
                    nix::sys::stat::minor(metadata.rdev()),
                ) != (device.major, device.minor)
            {
                return Err(Status::failed_precondition(
                    "device identity changed before release verification",
                ));
            }
            // This detects kernel filesystem holders across mount namespaces,
            // not arbitrary raw userspace fds, nor future privileged reopens.
            Ok(())
        })
        .await
        .map_err(|_| Status::internal("device release check worker failed"))?
    }

    async fn device_filesystem(&self, device: &Device) -> Result<Option<(u64, u64)>, Status> {
        let device = device.clone();
        tokio::task::spawn_blocking(move || {
            let metadata = match std::fs::metadata(&device.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_status("inspect device filesystem", error)),
            };
            if !metadata.file_type().is_block_device()
                || (
                    nix::sys::stat::major(metadata.rdev()),
                    nix::sys::stat::minor(metadata.rdev()),
                ) != (device.major, device.minor)
            {
                return Err(Status::failed_precondition(
                    "device identity changed before filesystem inspection",
                ));
            }
            Ok(Some((
                nix::sys::stat::major(metadata.dev()),
                nix::sys::stat::minor(metadata.dev()),
            )))
        })
        .await
        .map_err(|_| Status::internal("device filesystem worker failed"))?
    }

    async fn block_identity(&self, target: &Path) -> Result<Option<(u64, u64)>, Status> {
        let target = target.to_owned();
        tokio::task::spawn_blocking(move || {
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
        })
        .await
        .map_err(|_| Status::internal("block identity worker failed"))?
    }

    async fn usage(&self, target: &Path) -> Result<Usage, Status> {
        let target = target.to_owned();
        tokio::task::spawn_blocking(move || {
            let stat = nix::sys::statvfs::statvfs(&target).map_err(|e| io_status("statvfs", e))?;
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
        })
        .await
        .map_err(|_| Status::internal("volume usage worker failed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_handles_optional_fields_and_escaped_paths() {
        let result = parse_mountinfo(b"36 25 8:17 / /var/lib/kubelet/a\\040b rw,nosuid,nodev shared:7 master:2 - xfs /dev/vdb rw,attr2\n").unwrap();
        assert_eq!(result[0].target, Path::new("/var/lib/kubelet/a b"));
        assert_eq!((result[0].major, result[0].minor), (8, 17));
        assert!(result[0].options.contains("nodev"));
    }

    #[test]
    fn malformed_mountinfo_fails_closed() {
        assert!(parse_mountinfo(b"not mountinfo").is_err());
        assert!(parse_mountinfo(b"36 25 8:1 / /bad\\999 rw - xfs /dev/x rw").is_err());
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

    fn scratch() -> tempfile::TempDir {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("host-tests");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    #[test]
    fn mountinfo_preserves_non_utf8_paths_and_empty_source() {
        use std::os::unix::ffi::OsStrExt;
        let mounts = parse_mountinfo(b"36 25 8:17 /old\xff /target\xfe rw - ext4 /source\xfd rw\n37 25 0:8 / /empty rw - tmpfs  rw\n").unwrap();
        assert_eq!(mounts[0].root.as_os_str().as_bytes(), b"/old\xff");
        assert_eq!(mounts[0].target.as_os_str().as_bytes(), b"/target\xfe");
        assert_eq!(mounts[1].target, Path::new("/empty"));
        assert_eq!(mounts[1].fs_type, "tmpfs");
        assert!(parse_mountinfo(b"36 25 8:17 / /x rw - ext4 rw\n").is_err());
    }

    fn scsi_fixture(base: &Path, bus: &str, driver: &str) -> PathBuf {
        use std::os::unix::fs::symlink;
        let transport = base.join("topology/virtio0");
        let device = transport.join("host0/target0:0:0/0:0:0:0");
        let root = base.join("block/sda");
        for path in [
            &device,
            &root,
            &base.join("buses/scsi"),
            &base.join("buses").join(bus),
            &base.join("drivers").join(driver),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        symlink(&device, root.join("device")).unwrap();
        symlink(base.join("buses/scsi"), device.join("subsystem")).unwrap();
        symlink(base.join("buses").join(bus), transport.join("subsystem")).unwrap();
        symlink(base.join("drivers").join(driver), transport.join("driver")).unwrap();
        std::fs::write(device.join("type"), b"0\n").unwrap();
        root
    }

    #[tokio::test]
    async fn transport_requires_scsi_disk_on_virtio_scsi() {
        for (bus, driver, accepted) in [
            ("virtio", "virtio_scsi", true),
            ("pci", "ahci", false),
            ("virtio", "virtio_blk", false),
            ("pci", "nvme", false),
        ] {
            let directory = scratch();
            let root = scsi_fixture(directory.path(), bus, driver);
            assert_eq!(
                require_virtio_scsi(&root).await.is_ok(),
                accepted,
                "{bus}/{driver}"
            );
            std::fs::write(root.join("device/type"), b"5\n").unwrap();
            assert!(
                require_virtio_scsi(&root).await.is_err(),
                "SCSI CD-ROM is not a disk"
            );
        }
    }

    #[tokio::test]
    async fn exclusive_release_rejects_a_regular_file() {
        let directory = scratch();
        let path = directory.path().join("ordinary");
        File::create(&path).unwrap();
        let host = LinuxHost {
            device_root: directory.path().into(),
            sys_block_dir: directory.path().into(),
            process_lock: Arc::new(File::open(&path).unwrap()),
        };
        let error = host
            .ensure_released(&Device {
                path,
                major: 8,
                minor: 0,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn command_inherits_lock_even_if_waiter_is_abandoned() {
        struct Finish(PathBuf);
        impl Drop for Finish {
            fn drop(&mut self) {
                let _ = File::create(&self.0);
            }
        }
        let directory = scratch();
        let lock_path = directory.path().join("node.lock");
        let lock = File::create(&lock_path).unwrap();
        // SAFETY: flock only operates on the valid owned fd; no explicit unlock
        // is performed because children must retain the same open-file lock.
        assert_eq!(
            unsafe { nix::libc::flock(lock.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) },
            0
        );
        let host = LinuxHost {
            device_root: directory.path().into(),
            sys_block_dir: directory.path().into(),
            process_lock: Arc::new(lock),
        };
        let ready = directory.path().join("ready");
        let finish = Finish(directory.path().join("finish"));
        let args = vec![
            "-c".into(),
            "touch \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.01; done".into(),
            "lock-test".into(),
            ready.as_os_str().into(),
            finish.0.as_os_str().into(),
        ];
        let worker = tokio::spawn(async move { host.command("sh", &args).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.abort();
        let _ = worker.await;
        let retry = File::open(lock_path).unwrap();
        // SAFETY: the retry fd is valid and remains alive throughout this test.
        assert_eq!(
            unsafe { nix::libc::flock(retry.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(nix::libc::EWOULDBLOCK)
        );
        drop(finish);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                // SAFETY: same valid retry fd; successful lock releases on drop.
                if unsafe {
                    nix::libc::flock(retry.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB)
                } == 0
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn device_filesystem_requires_identity_and_handles_a_missing_node() {
        let directory = scratch();
        let path = directory.path().join("ordinary");
        let lock = Arc::new(File::create(directory.path().join("lock")).unwrap());
        let host = LinuxHost {
            device_root: directory.path().into(),
            sys_block_dir: directory.path().into(),
            process_lock: lock,
        };
        let device = Device {
            path: path.clone(),
            major: 8,
            minor: 0,
        };
        assert_eq!(host.device_filesystem(&device).await.unwrap(), None);
        File::create(path).unwrap();
        assert_eq!(
            host.device_filesystem(&device).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
    }
    #[tokio::test]
    async fn missing_device_path_requires_proof_that_sysfs_device_is_gone() {
        let directory = scratch();
        let sysfs = directory.path().join("sys-block");
        std::fs::create_dir_all(sysfs.join("sda")).unwrap();
        let host = LinuxHost {
            device_root: directory.path().into(),
            sys_block_dir: sysfs.clone(),
            process_lock: Arc::new(File::create(directory.path().join("lock")).unwrap()),
        };
        let device = Device {
            path: directory.path().join("missing"),
            major: 8,
            minor: 0,
        };
        std::fs::write(sysfs.join("sda/dev"), b"8:0\n").unwrap();
        assert_eq!(
            host.ensure_released(&device).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        std::fs::write(sysfs.join("sda/dev"), b"8:16\n").unwrap();
        assert!(host.ensure_released(&device).await.is_ok());
        std::fs::write(sysfs.join("sda/dev"), b"invalid\n").unwrap();
        assert_eq!(
            host.ensure_released(&device).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        std::fs::remove_file(sysfs.join("sda/dev")).unwrap();
        // A vanished unrelated entry is not evidence of an unknown identity.
        assert!(host.ensure_released(&device).await.is_ok());
        std::fs::remove_dir_all(&sysfs).unwrap();
        assert_eq!(
            host.ensure_released(&device).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }
}
