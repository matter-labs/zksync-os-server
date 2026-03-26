use std::path::{Path, PathBuf};

// Include generated include_bytes! constants and app_bin_bytes() lookup.
include!(concat!(env!("OUT_DIR"), "/apps_generated.rs"));

fn materialize_app(base_dir: &Path, tag: &str, file_name: &str, bytes: &[u8]) -> PathBuf {
    let dir_path = base_dir.join(tag);
    std::fs::create_dir_all(&dir_path).unwrap();

    let full_path = dir_path.join(file_name);
    if !full_path.exists() {
        std::fs::write(&full_path, bytes).unwrap();
    }
    full_path
}

// pub for generated versioned submodules (v5, v6, ...) to call.
pub fn resolve(tag: &str, variant: &str, base_dir: &Path) -> PathBuf {
    let bytes = app_bin_bytes(tag, variant)
        .unwrap_or_else(|| panic!("unknown app_bin_tag/variant: {tag}/{variant}"));
    materialize_app(base_dir, tag, &format!("{variant}.bin"), bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths_are_scoped_to_the_requested_base_dir() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let path_a = v5::singleblock_batch_path(dir_a.path());
        let path_b = v5::singleblock_batch_path(dir_b.path());
        assert_ne!(path_a, path_b);
        assert!(path_a.exists());
        assert!(path_b.exists());
    }
}
