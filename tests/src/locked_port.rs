use std::fs::File;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

use anyhow::{Context, Result};
use fs2::FileExt;

/// A free TCP port held by an exclusive file lock in `$TMPDIR`.
///
/// The lock prevents other test workers on the same machine from picking the
/// same port number. Holding the lock until the server successfully binds
/// eliminates the same-process TOCTOU race in `ChainRuntime::allocate`.
pub struct LockedPort {
    pub port: u16,
    _lockfile: File,
}

impl LockedPort {
    pub fn acquire_unused() -> Result<Self> {
        for _ in 0..100 {
            let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .context("bind port 0")?;
            let port = listener.local_addr()?.port();
            drop(listener);
            match Self::try_lock(port) {
                Ok(locked) => return Ok(locked),
                Err(_) => continue,
            }
        }
        anyhow::bail!("failed to acquire an unused port after 100 attempts")
    }

    fn try_lock(port: u16) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("zksync-os-port{port}.lock"));
        let file = match File::create(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                anyhow::bail!("permission denied creating lockfile for port {port}")
            }
            Err(e) => return Err(e).with_context(|| format!("create lockfile for port {port}")),
        };
        if file.try_lock_exclusive().is_ok() {
            Ok(Self {
                port,
                _lockfile: file,
            })
        } else {
            anyhow::bail!("port {port} is already locked by another test worker")
        }
    }
}

impl Drop for LockedPort {
    fn drop(&mut self) {
        self._lockfile
            .unlock()
            .unwrap_or_else(|e| panic!("failed to unlock port {} lockfile: {e}", self.port));
    }
}
