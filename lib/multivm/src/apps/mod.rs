pub mod v6 {
    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V6_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V6_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V6_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));
}

pub mod v7 {
    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V7_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V7_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_V7_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));
}
