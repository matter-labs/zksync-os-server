//! ZKsync-specific extensions to the alloy ecosystem.
//!
//! - [`network`]: the [`alloy::network::Network`] implementations for ZKsync OS (re-exported
//!   from `zksync_os_provider`, which is their source of truth).
//! - [`provider`]: the [`provider::ZksyncApi`] trait exposing `zks_*` RPC methods.

pub mod provider;

pub use zksync_os_provider::network;
