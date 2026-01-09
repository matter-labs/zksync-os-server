//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// Current protocol version for local chain configuration.
pub const PROTOCOL_VERSION: &str = "v31";

/// Private key to update base token price on L1.
/// Must be consistent with the key set on the chain admin contract.
#[rustfmt::skip]
pub const TOKEN_MULTIPLIER_SETTER_PK: &str = "";
