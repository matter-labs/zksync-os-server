/// App binaries of the zksync-os 0.2.x proving lane (protocol v30.1/v30.2).
pub mod v0_2 {
    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_2_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_2_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_2_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));
}

/// App binaries of the zksync-os 0.3.x proving lane (protocol v31.0/v31.1).
pub mod v0_3 {
    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_3_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_3_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZK_OS_0_3_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));
}
