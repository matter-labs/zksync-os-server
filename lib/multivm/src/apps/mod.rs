pub mod v3 {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26-rc1_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub fn singleblock_batch_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("singleblock_batch.bin");
            std::fs::write(&full_path, SINGLEBLOCK_BATCH_APP).unwrap();
            full_path
        })
        .clone()
    }

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26-rc1_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub fn singleblock_batch_logging_enabled_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("singleblock_batch_logging_enabled.bin");
            std::fs::write(&full_path, SINGLEBLOCK_BATCH_LOGGING_ENABLED).unwrap();
            full_path
        })
        .clone()
    }

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_0_26-rc1_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));

    pub fn multiblock_batch_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("multiblock_batch.bin");
            std::fs::write(&full_path, MULTIBLOCK_BATCH).unwrap();
            full_path
        })
        .clone()
    }
}

pub mod v4 {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_1_0_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub fn singleblock_batch_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("singleblock_batch.bin");
            std::fs::write(&full_path, SINGLEBLOCK_BATCH_APP).unwrap();
            full_path
        })
        .clone()
    }

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_1_0_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub fn singleblock_batch_logging_enabled_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("singleblock_batch_logging_enabled.bin");
            std::fs::write(&full_path, SINGLEBLOCK_BATCH_LOGGING_ENABLED).unwrap();
            full_path
        })
        .clone()
    }

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_0_1_0_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));

    pub fn multiblock_batch_path(base_dir: &Path) -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();

        PATH.get_or_init(|| {
            let dir_path = base_dir.join(
                module_path!()
                    .rsplit_once("::")
                    .expect("failed to get module name")
                    .1,
            );
            std::fs::create_dir_all(&dir_path).unwrap();

            let full_path = dir_path.join("multiblock_batch.bin");
            std::fs::write(&full_path, MULTIBLOCK_BATCH).unwrap();
            full_path
        })
        .clone()
    }
}
