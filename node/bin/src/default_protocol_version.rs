//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

// TODO: to be moved to config instead of constants
/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// Protocol version v30.2
pub const PROTOCOL_VERSION_V30_2: &str = "v30.2";

/// Protocol version v31.0
pub const PROTOCOL_VERSION_V31_0: &str = "v31.0";

/// Protocol version v32.0
pub const PROTOCOL_VERSION_V32_0: &str = "v32.0";

/// Current default protocol version for local chain configuration.
/// v30.x can no longer be launched with default settings: V6 proving support was dropped.
pub const PROTOCOL_VERSION: &str = PROTOCOL_VERSION_V31_0;

/// Next protocol version for local chain configuration.
/// Required for testing the upgrade process. No local-chain fixtures exist for it yet;
/// tests reach it by upgrading a `PROTOCOL_VERSION` chain in-test.
pub const NEXT_PROTOCOL_VERSION: &str = PROTOCOL_VERSION_V32_0;
