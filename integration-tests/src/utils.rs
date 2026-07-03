#[cfg(feature = "prover-tests")]
pub(crate) fn materialize_multiblock_batch_bin(
    base_dir: &std::path::Path,
    version: &str,
    bytes: &[u8],
) -> std::path::PathBuf {
    let dir_path = base_dir.join(version);
    std::fs::create_dir_all(&dir_path).unwrap();

    let full_path = dir_path.join("multiblock_batch.bin");
    if !full_path.exists() {
        std::fs::write(&full_path, bytes).unwrap();
    }
    full_path
}
