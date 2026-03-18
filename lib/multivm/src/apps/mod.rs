use std::path::{Path, PathBuf};

fn materialize_app(base_dir: &Path, version: &str, file_name: &str, bytes: &[u8]) -> PathBuf {
    let dir_path = base_dir.join(version);
    std::fs::create_dir_all(&dir_path).unwrap();

    let full_path = dir_path.join(file_name);
    if !full_path.exists() {
        std::fs::write(&full_path, bytes).unwrap();
    }
    full_path
}

/// App binaries from tag v0.2.5 (used by protocol versions 0.30.1, 0.30.2).
pub mod v6 {
    use std::path::{Path, PathBuf};

    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_V0_2_5_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub fn singleblock_batch_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(
            base_dir,
            "v6",
            "singleblock_batch.bin",
            SINGLEBLOCK_BATCH_APP,
        )
    }

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_V0_2_5_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub fn singleblock_batch_logging_enabled_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(
            base_dir,
            "v6",
            "singleblock_batch_logging_enabled.bin",
            SINGLEBLOCK_BATCH_LOGGING_ENABLED,
        )
    }

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_V0_2_5_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));

    pub fn multiblock_batch_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(base_dir, "v6", "multiblock_batch.bin", MULTIBLOCK_BATCH)
    }
}

/// App binaries from tag dev-20260311 (used by protocol versions 0.31.x, 0.32.x).
pub mod v7 {
    use std::path::{Path, PathBuf};

    pub const SINGLEBLOCK_BATCH_APP: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_DEV_20260311_SOURCE_PATH"),
        "/singleblock_batch.bin"
    ));

    pub fn singleblock_batch_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(
            base_dir,
            "v7",
            "singleblock_batch.bin",
            SINGLEBLOCK_BATCH_APP,
        )
    }

    pub const SINGLEBLOCK_BATCH_LOGGING_ENABLED: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_DEV_20260311_SOURCE_PATH"),
        "/singleblock_batch_logging_enabled.bin"
    ));

    pub fn singleblock_batch_logging_enabled_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(
            base_dir,
            "v7",
            "singleblock_batch_logging_enabled.bin",
            SINGLEBLOCK_BATCH_LOGGING_ENABLED,
        )
    }

    pub const MULTIBLOCK_BATCH: &[u8] = include_bytes!(concat!(
        env!("ZKSYNC_OS_APPS_DEV_20260311_SOURCE_PATH"),
        "/multiblock_batch.bin"
    ));

    pub fn multiblock_batch_path(base_dir: &Path) -> PathBuf {
        super::materialize_app(base_dir, "v7", "multiblock_batch.bin", MULTIBLOCK_BATCH)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use test_casing::test_casing;

    const PATH_FNS: [fn(&Path) -> PathBuf; 2] = [
        super::v6::singleblock_batch_path,
        super::v7::singleblock_batch_path,
    ];

    #[test_casing(2, PATH_FNS)]
    fn app_paths_are_scoped_to_the_requested_base_dir(path_fn: fn(&Path) -> PathBuf) {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let path_a = path_fn(dir_a.path());
        let path_b = path_fn(dir_b.path());
        assert_ne!(path_a, path_b);
        assert!(path_a.exists());
        assert!(path_b.exists());
    }
}
