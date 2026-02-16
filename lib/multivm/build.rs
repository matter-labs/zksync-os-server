use cargo_metadata::{MetadataCommand, PackageId};
use url::Url;

fn parse_git_ref(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let mut query_pairs_clone = url.query_pairs();
    let (_, git_ref) = query_pairs
        .find(|(key, _)| key == "branch")
        .or_else(|| query_pairs_clone.find(|(key, _)| key == "tag"))
        .ok_or_else(|| anyhow::anyhow!("missing branch / tag in git url `{url}`"))?;
    Ok(git_ref.to_string())
}

fn version_key(git_ref: &str) -> String {
    // Branches like `vv-0-2-5-new-rust-toolchain` and tags like `v0.2.5` both map to `0_2_5`.
    let numeric_parts = git_ref
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(3)
        .collect::<Vec<_>>();
    if numeric_parts.len() == 3 {
        return numeric_parts.join("_");
    }

    git_ref
        .trim_start_matches("v")
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let metadata = MetadataCommand::new().exec().unwrap();

    // Find forward_system crate and expose its path to the directory containing `app*.bin` files.
    for package in &metadata.packages {
        if package.name.as_str() != "forward_system" {
            continue;
        }
        let git_ref = match parse_git_ref(&package.id) {
            Ok(git_ref) => git_ref,
            Err(err) => {
                println!("cargo::error=failed to parse forward_system's git ref: {err}");
                return;
            }
        };

        let dir = format!("{manifest_dir}/apps/{git_ref}");
        std::fs::create_dir_all(&dir).expect("failed to create directory");
        for variant in [
            "multiblock_batch",
            "singleblock_batch",
            "singleblock_batch_logging_enabled",
        ] {
            let url = format!(
                "https://github.com/matter-labs/zksync-os/releases/download/{git_ref}/{variant}.bin"
            );
            let path = format!("{dir}/{variant}.bin");
            if std::fs::exists(&path).expect("failed to check file existence") {
                continue;
            }
            let resp = reqwest::blocking::get(url).expect("failed to download");
            let body = resp.bytes().expect("failed to read response body").to_vec();
            std::fs::write(path, body).expect("failed to write file");
        }

        let snake_case_version = version_key(&git_ref);
        println!("cargo:rustc-env=ZKSYNC_OS_{snake_case_version}_SOURCE_PATH={dir}");
    }
}
