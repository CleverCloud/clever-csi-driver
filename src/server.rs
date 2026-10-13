//! Exclusive Unix-socket ownership, including safe recovery after a killed process.
use nix::fcntl::{Flock, FlockArg};
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;

pub struct SocketGuard {
    path: PathBuf,
    inode: u64,
    _lock: Flock<File>,
}

impl SocketGuard {
    pub fn is_current(&self) -> bool {
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.ino() == self.inode)
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Never remove another process's replacement socket.
        if self.is_current() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn endpoint_path(endpoint: &str) -> io::Result<PathBuf> {
    let path = endpoint.strip_prefix("unix://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "CSI endpoint must use unix:///absolute/path",
        )
    })?;
    let path = PathBuf::from(path);
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CSI socket must have an absolute path without '..'",
        ));
    }
    Ok(path)
}

pub fn bind_socket(path: &Path) -> io::Result<(UnixListener, SocketGuard)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket needs parent directory")
    })?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("sock.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(lock_path)?;
    let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another driver owns this CSI socket: {e}"),
        )
    })?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "refusing to replace a non-socket CSI endpoint",
                ));
            }
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "CSI endpoint already has a live server",
                    ));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)?
                }
                Err(e) => return Err(e),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    let inode = std::fs::symlink_metadata(path)?.ino();
    Ok((
        listener,
        SocketGuard {
            path: path.into(),
            inode,
            _lock: lock,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn refuses_live_server_and_recovers_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("csi.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let (listener, guard) = bind_socket(&path).unwrap();
        assert_eq!(
            bind_socket(&path).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        drop(listener);
        drop(guard);
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn detects_removed_or_replaced_socket_without_removing_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("csi.sock");
        let (_listener, guard) = bind_socket(&path).unwrap();
        assert!(guard.is_current());
        std::fs::remove_file(&path).unwrap();
        assert!(!guard.is_current());
        let _replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(!guard.is_current());
        drop(guard);
        assert!(path.exists());
    }
    #[tokio::test]
    async fn never_deletes_regular_files_or_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("csi.sock");
        std::fs::write(&path, "user data").unwrap();
        assert!(bind_socket(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user data");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(bind_socket(&link).is_err());
    }
}
