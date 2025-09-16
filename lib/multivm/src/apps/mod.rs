pub mod v1 {
    pub const SERVER_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_22_SOURCE_PATH"),
        "/server_app.bin"
    ));
    pub const SERVER_APP_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_22_SOURCE_PATH"),
        "/server_app_logging_enabled.bin"
    ));
    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_22_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));
}

pub fn create_temp_file(data: &[u8]) -> std::io::Result<tempfile::NamedTempFile> {
    let mut temp_file = tempfile::NamedTempFile::new()?;
    use std::io::Write;
    temp_file.write_all(data)?;
    Ok(temp_file)
}
