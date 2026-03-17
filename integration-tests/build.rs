use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::str::from_utf8;

/// Decompress `l1-state.json.gz` files at build time so every test process can
/// read the plain JSON without paying the ~70 MB decompression cost at runtime.
///
/// A `.sha256` sidecar file stores the hash of the `.gz` input; the
/// decompressed output is only regenerated when the hash changes.
fn decompress_l1_states() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir).parent().unwrap();
    let local_chains = workspace_root.join("local-chains");

    // Re-run when a version directory is added/removed or any .gz file changes.
    println!("cargo::rerun-if-changed={}", local_chains.display());

    let Ok(entries) = std::fs::read_dir(&local_chains) else {
        return;
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }

        let gz_path = entry.path().join("l1-state.json.gz");
        assert!(gz_path.is_file(), "expected {} to exist", gz_path.display());

        let compressed = std::fs::read(&gz_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", gz_path.display()));

        let hash = Sha256::digest(&compressed);
        let hex_hash = format!("{hash:x}");

        // l1-state.json.gz → l1-state.json (with_extension strips last extension)
        let json_path = gz_path.with_extension("");
        let hash_path = json_path.with_extension("json.sha256");

        // Skip decompression if the output exists and the hash file matches.
        if json_path.is_file()
            && let Ok(existing_hash) = std::fs::read_to_string(&hash_path)
            && existing_hash.trim() == hex_hash
        {
            continue;
        }

        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut decoded = Vec::new();
        decoder
            .read_to_end(&mut decoded)
            .unwrap_or_else(|e| panic!("failed to decompress {}: {e}", gz_path.display()));

        std::fs::write(&json_path, &decoded)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", json_path.display()));
        std::fs::write(&hash_path, &hex_hash)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", hash_path.display()));
    }
}

fn main() {
    decompress_l1_states();

    // Rerun build script when test contracts change.
    println!("cargo::rerun-if-changed=test-contracts/src");
    println!("cargo::rerun-if-changed=test-contracts/foundry.toml");

    // Check that `forge` is installed and is executable
    let Ok(status) = Command::new("forge").arg("--version").status() else {
        println!("cargo::warning=`forge` not found, skipping build script");
        println!("cargo::warning=visit https://getfoundry.sh/ for installation instructions");
        return;
    };
    if !status.success() {
        println!("cargo::warning=could not run `forge --version`, skipping build script");
        println!("cargo::warning=make sure your foundry installation is working correctly");
        return;
    }

    match Command::new("forge")
        .arg("build")
        .arg("--root")
        .arg("test-contracts")
        .output()
    {
        Ok(output) if output.status.success() => {
            // Success, do nothing
        }
        Ok(output) => {
            println!("cargo::error=`forge build` failed, see stdout/stderr below");
            println!("cargo::error=stdout={}", from_utf8(&output.stdout).unwrap());
            println!("cargo::error=stderr={}", from_utf8(&output.stderr).unwrap());
        }
        Err(err) => {
            println!("cargo::error=could not run `forge build`: {err}");
        }
    }
}
