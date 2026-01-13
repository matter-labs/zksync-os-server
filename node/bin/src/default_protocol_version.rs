//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

// TODO: to be moved to config instead of constants
/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// Current default protocol version for local chain configuration.
pub const PROTOCOL_VERSION: &str = "v30.2";
