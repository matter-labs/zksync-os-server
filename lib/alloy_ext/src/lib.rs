//! ZKsync-specific extensions to the alloy ecosystem.
//!
//! - [`network`]: the [`alloy::network::Network`] implementation for ZKsync OS.
//! - [`provider`]: the [`provider::ZksyncApi`] trait exposing `zks_*` RPC methods.
//!
//! The object-safe, wallet-capable provider wrapper lives in the `zksync_os_provider` crate as
//! `NodeProvider` (it had to move below `contract_interface` so the latter can be generic over it).

pub mod network;
pub mod provider;
