pub mod v3 {
    use alloy::hex;
    use alloy::primitives::B256;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    pub const SERVER_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26_SOURCE_PATH"),
        "/server_app.bin"
    ));

    pub fn server_app_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join("v3");
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("server_app.bin");
            std::fs::write(&full_path, SERVER_APP).unwrap();
            full_path
        })
        .clone()
    }

    pub const SERVER_APP_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26_SOURCE_PATH"),
        "/server_app_logging_enabled.bin"
    ));

    pub fn server_app_logging_enabled_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join("v3");
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("server_app_logging_enabled.bin");
            std::fs::write(&full_path, SERVER_APP_LOGGING_ENABLED).unwrap();
            full_path
        })
        .clone()
    }

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));

    pub fn multiblock_batch_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join("v3");
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("multiblock_batch.bin");
            std::fs::write(&full_path, MULTIBLOCK_BATCH).unwrap();
            full_path
        })
        .clone()
    }

    pub const VERIFICATION_KEY_HASH: B256 = {
        let vk_hash_hex = include_bytes!(concat!(
            env!("ZKSYNC_OS_0_0_26_SOURCE_PATH"),
            "/snark_vk_hash.txt"
        ));
        match hex::const_decode_to_array::<32>(vk_hash_hex) {
            Ok(hash) => B256::new(hash),
            Err(_) => {
                panic!("malformed VK hash")
            }
        }
    };
}
