//! Support for representing the version of the `zks` protocol.
//!
//! Version history:
//! - `zks/0` — test-only, never registered in production.
//! - `zks/1`–`zks/4` — retired and removed. `zks/1`/`zks/2` were replay-only on older record
//!   encodings; `zks/3`/`zks/4` additionally carried the verifier messages inline (message IDs
//!   0x02–0x06) before those moved to the standalone `zks_2fa` subprotocol.
//! - `zks/5` — current version; replay-only, `v3` record encoding.
//!
//! Versions evolve additively and are removed only in a later breaking release once the whole
//! fleet speaks a newer one; retired version numbers are never reused. See the "Version
//! lifecycle" section in `docs/src/design/devp2p.md` before touching this file.

use crate::wire::replays::{WireReplayRecord, v0, v3};
use std::fmt::Debug;

/// Type-level specification for a `zks` protocol version and its pinned wire formats.
pub trait ZksProtocolVersionSpec: Debug + Send + Sync + Unpin + Clone + 'static {
    /// Wire format for replay record.
    type Record: WireReplayRecord;

    /// Version number matching this protocol version.
    const VERSION: ZksVersion;
}

/// Test-only protocol version whose replay record preserves just the block number.
///
/// Keeping this deliberately lossy version makes capability-negotiation tests able to observe
/// which version the peers selected.
#[derive(Debug, Clone)]
pub struct ZksProtocolV0;

impl ZksProtocolVersionSpec for ZksProtocolV0 {
    type Record = v0::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks0;
}

/// Protocol version 5 supports replay streaming only via `GetBlockReplays` and `BlockReplays`.
/// The verifier handshake and batch verification live in the standalone `zks_2fa` subprotocol.
#[derive(Debug, Clone)]
pub struct ZksProtocolV5;

impl ZksProtocolVersionSpec for ZksProtocolV5 {
    type Record = v3::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks5;
}

/// Error returned when a byte does not identify a registered [`ZksVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Unknown zks protocol version: {0}")]
pub struct ParseVersionError(String);

/// The `zks` protocol version.
///
/// Discriminants are the version numbers advertised in the RLPx capability list. Versions 1-4 are
/// retired and their numbers must not be reused.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ZksVersion {
    /// The `zks` protocol version 0. Only used for testing.
    Zks0 = 0,
    /// The `zks` protocol version 5. Replay-only; verifier messages moved to `zks_2fa`.
    Zks5 = 5,
}

impl ZksVersion {
    /// The latest registered `zks` version.
    pub const LATEST: Self = Self::Zks5;
}

/// Converts a `u8` into a registered [`ZksVersion`].
///
/// # Example
/// ```
/// use zksync_os_network::version::ZksVersion;
///
/// let version = ZksVersion::try_from(5).unwrap();
/// assert_eq!(version, ZksVersion::Zks5);
/// ```
impl TryFrom<u8> for ZksVersion {
    type Error = ParseVersionError;

    #[inline]
    fn try_from(u: u8) -> Result<Self, Self::Error> {
        match u {
            0 => Ok(Self::Zks0),
            5 => Ok(Self::Zks5),
            _ => Err(ParseVersionError(u.to_string())),
        }
    }
}

impl From<ZksVersion> for u8 {
    #[inline]
    fn from(v: ZksVersion) -> Self {
        v as Self
    }
}

#[cfg(test)]
mod tests {
    use super::ZksVersion;

    #[test]
    fn discriminants_match_advertised_capability_versions() {
        assert_eq!(ZksVersion::Zks0 as u8, 0);
        assert_eq!(ZksVersion::Zks5 as u8, 5);
    }

    #[test]
    fn try_from_rejects_retired_and_unknown_versions() {
        assert_eq!(ZksVersion::try_from(0), Ok(ZksVersion::Zks0));
        assert_eq!(ZksVersion::try_from(5), Ok(ZksVersion::Zks5));
        for retired in 1..=4u8 {
            assert!(ZksVersion::try_from(retired).is_err());
        }
        assert!(ZksVersion::try_from(6).is_err());
    }
}
