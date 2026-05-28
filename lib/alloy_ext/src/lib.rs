//! ZKsync-specific extensions to the alloy ecosystem.
//!
//! - [`network`]: the [`alloy::network::Network`] implementation for ZKsync OS.
//! - [`dyn_wallet_provider`]: an object-safe wrapper over [`alloy::providers::Provider<Ethereum>`].
//! - [`provider`]: the [`provider::ZksyncApi`] trait exposing `zks_*` RPC methods.

pub mod dyn_wallet_provider;
pub mod network;
pub mod provider;
