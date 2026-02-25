use cargo_metadata::{MetadataCommand, PackageId};
use url::Url;

fn parse_git_tag(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, tag) = query_pairs
        .find(|(key, _)| key == "tag")
        .ok_or_else(|| anyhow::anyhow!("missing tag in git url `{url}`"))?;
    Ok(tag.to_string())
}

fn proving_version_from_tag(tag: &str) -> Option<String> {
    match tag {
        "v0.2.7-interface-v0.0.13" => Some(String::from("V6")),
        _ => None,
    }
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let metadata = MetadataCommand::new().exec().unwrap();

    // Find forward_system crate and expose its path to the directory containing `app*.bin` files.
    for package in &metadata.packages {
        if package.name.as_str() != "forward_system" {
            continue;
        }
        let tag = match parse_git_tag(&package.id) {
            Ok(tag) => tag,
            Err(err) => {
                println!("cargo::error=failed to parse forward_system's git tag: {err}");
                return;
            }
        };

        if let Some(proving_version) = proving_version_from_tag(&tag) {
            let dir = format!("{manifest_dir}/apps/{tag}");
            std::fs::create_dir_all(&dir).expect("failed to create directory");
            for variant in [
                "multiblock_batch",
                "singleblock_batch",
                "singleblock_batch_logging_enabled",
            ] {
                let url = format!(
                    "https://github.com/matter-labs/zksync-os/releases/download/{tag}/{variant}.bin"
                );
                let path = format!("{dir}/{variant}.bin");
                if std::fs::exists(&path).expect("failed to check file existence") {
                    continue;
                }
                let resp = reqwest::blocking::get(url).expect("failed to download");
                let body = resp.bytes().expect("failed to read response body").to_vec();
                std::fs::write(path, body).expect("failed to write file");
            }

            println!("cargo:rustc-env=ZKSYNC_OS_{proving_version}_SOURCE_PATH={dir}");
            continue;
        }
    }
}
