use anyhow::Context;
use fs2::FileExt;
use std::{
    fs::File,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    time::Duration,
};
use tokio::net::TcpListener;

const UNUSED_PORT_RETRY_ATTEMPTS: usize = 1_000;
const UNUSED_PORT_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub struct LockedPort {
    pub port: u16,
    lockfile: File,
}

impl LockedPort {
    /// Checks if the requested port is free.
    /// Returns the unused port (same value as input, except for `0`).
    pub(crate) async fn check_port_is_unused(port: u16) -> anyhow::Result<u16> {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let tcp_listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind to port={port}"))?;
        let port = tcp_listener
            .local_addr()
            .context("failed to get local address for random port")?
            .port();
        let udp_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .with_context(|| format!("failed to bind UDP socket to port={port}"))?;
        drop(udp_socket);
        Ok(port)
    }

    /// Request an unused port from the OS.
    async fn pick_unused_port() -> anyhow::Result<u16> {
        // Port 0 means the OS gives us an unused port
        Self::check_port_is_unused(0).await
    }

    /// Acquire an unused port and lock it (meaning no other competing callers of this method can
    /// take this lock). Lock lasts until the returned `LockedPort` instance is dropped.
    pub async fn acquire_unused() -> anyhow::Result<Self> {
        let mut last_error = None;
        for _ in 0..UNUSED_PORT_RETRY_ATTEMPTS {
            match Self::pick_unused_port().await {
                Ok(port) => match Self::try_lock(port).await {
                    Ok(locked_port) => return Ok(locked_port),
                    Err(error) => last_error = Some(error),
                },
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(UNUSED_PORT_RETRY_INTERVAL).await;
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no unused port acquisition attempted")))
            .context("failed to acquire an unused port")
    }

    /// Acquire a specific port and lock it. Lock lasts until the returned `LockedPort` is dropped.
    pub async fn acquire(port: u16) -> anyhow::Result<Self> {
        Self::try_lock(port).await
    }

    async fn try_lock(port: u16) -> anyhow::Result<Self> {
        let port = Self::check_port_is_unused(port).await?;
        let lockpath = std::env::temp_dir().join(format!("zksync-os-port{port}.lock"));
        let lockfile = match File::create(lockpath) {
            Ok(lockfile) => lockfile,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                anyhow::bail!("failed to create lockfile for port={port}: permission denied");
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to create lockfile for port={port}"));
            }
        };
        if lockfile.try_lock_exclusive().is_ok() {
            Ok(Self { port, lockfile })
        } else {
            anyhow::bail!("failed to lock port={port}")
        }
    }
}

/// Dropping `LockedPort` unlocks the port, caller needs to make sure the port is already bound to
/// or is not needed anymore.
impl Drop for LockedPort {
    fn drop(&mut self) {
        fs2::FileExt::unlock(&self.lockfile)
            .with_context(|| format!("failed to unlock lockfile for port={}", self.port))
            .unwrap();
    }
}

#[cfg(feature = "prover-tests")]
pub(crate) fn materialize_multiblock_batch_bin(
    base_dir: &std::path::Path,
    version: &str,
    bytes: &[u8],
) -> std::path::PathBuf {
    let dir_path = base_dir.join(version);
    std::fs::create_dir_all(&dir_path).unwrap();

    let full_path = dir_path.join("multiblock_batch.bin");
    if !full_path.exists() {
        std::fs::write(&full_path, bytes).unwrap();
    }
    full_path
}
