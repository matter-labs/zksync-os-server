use anyhow::Context;
use fs2::FileExt;
use std::{
    fs::File,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    ops::RangeInclusive,
    process,
    sync::LazyLock,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::net::TcpListener;

const UNUSED_PORT_RETRY_ATTEMPTS: usize = 1_000;
const UNUSED_PORT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const TEST_PORT_MIN: u16 = 10_000;
const TEST_PORT_MAX: u16 = u16::MAX;

static NEXT_PORT_ATTEMPT: AtomicUsize = AtomicUsize::new(0);
static TEST_PORT_RANGES: LazyLock<Vec<RangeInclusive<u16>>> =
    LazyLock::new(non_ephemeral_test_port_ranges);

#[derive(Debug)]
pub struct LockedPort {
    pub port: u16,
    lockfile: File,
}

impl LockedPort {
    /// Checks if the requested port is free.
    /// Returns the unused port (same value as input, except for `0`).
    pub(crate) async fn check_port_is_unused(port: u16) -> anyhow::Result<u16> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        let tcp_listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind to port={port}"))?;
        let port = tcp_listener
            .local_addr()
            .context("failed to get local address for random port")?
            .port();
        let udp_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
            .with_context(|| format!("failed to bind UDP socket to port={port}"))?;
        drop(udp_socket);
        Ok(port)
    }

    /// Pick a candidate outside the OS ephemeral range when that range is known.
    fn pick_unused_port(attempt: usize) -> u16 {
        let ranges = TEST_PORT_RANGES.as_slice();
        let port_count = ranges
            .iter()
            .map(|range| *range.end() as usize - *range.start() as usize + 1)
            .sum::<usize>();
        let seed = process::id() as usize;
        let mut offset = (seed + attempt * 7919) % port_count;

        for range in ranges {
            let range_len = *range.end() as usize - *range.start() as usize + 1;
            if offset < range_len {
                return *range.start() + offset as u16;
            }
            offset -= range_len;
        }

        unreachable!("non-empty test port ranges must yield a port")
    }

    /// Acquire an unused port and lock it (meaning no other competing callers of this method can
    /// take this lock). Lock lasts until the returned `LockedPort` instance is dropped.
    pub async fn acquire_unused() -> anyhow::Result<Self> {
        let mut last_error = None;
        for _ in 0..UNUSED_PORT_RETRY_ATTEMPTS {
            let port_attempt = NEXT_PORT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
            match Self::try_lock(Self::pick_unused_port(port_attempt)).await {
                Ok(locked_port) => return Ok(locked_port),
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

fn non_ephemeral_test_port_ranges() -> Vec<RangeInclusive<u16>> {
    let Some((ephemeral_start, ephemeral_end)) = linux_ephemeral_port_range() else {
        return vec![TEST_PORT_MIN..=TEST_PORT_MAX];
    };

    let mut ranges = Vec::new();
    if TEST_PORT_MIN < ephemeral_start {
        ranges.push(TEST_PORT_MIN..=ephemeral_start.saturating_sub(1));
    }
    if ephemeral_end < TEST_PORT_MAX {
        ranges.push(ephemeral_end.saturating_add(1)..=TEST_PORT_MAX);
    }

    if ranges.is_empty() {
        ranges.push(TEST_PORT_MIN..=TEST_PORT_MAX);
    }
    ranges
}

fn linux_ephemeral_port_range() -> Option<(u16, u16)> {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
    let mut ports = range.split_whitespace().map(str::parse::<u16>);
    let start = ports.next()?.ok()?;
    let end = ports.next()?.ok()?;
    (start <= end).then_some((start, end))
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
