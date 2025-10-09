use cargo_metadata::{MetadataCommand, PackageId};
use execution_utils::RecursionStrategy;
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use url::Url;

fn parse_git_tag(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, tag) = query_pairs
        .find(|(key, _)| key == "tag")
        .ok_or_else(|| anyhow::anyhow!("missing tag in git url `{url}`"))?;
    Ok(tag.to_string())
}

fn generate_vk(tag: &str, setup_key_path: PathBuf, dir: PathBuf) {
    let snark_vk_expected_path = dir.join("snark_vk_expected.json");
    let snark_vk_hash_path = dir.join("snark_vk_hash.txt");
    if snark_vk_expected_path.exists() && snark_vk_hash_path.exists() {
        // VKs already exist, skip generation
        return;
    }

    eprintln!(
        "missing SNARK verification keys for {tag}; generating now - this can take a few minutes"
    );
    std::io::stderr().flush().unwrap();

    let handle = thread::Builder::new()
        .name("vk-generation".into())
        .stack_size(128 * 1024 * 1024)
        .spawn(move || {
            zkos_wrapper::generate_vk(
                Some(
                    dir.join("multiblock_batch.bin")
                        .to_string_lossy()
                        .to_string(),
                ),
                dir.to_string_lossy().to_string(),
                Some(setup_key_path.to_string_lossy().to_string()),
                true,
                RecursionStrategy::UseReducedLog23Machine,
            )
            .expect("failed to generate vk")
        })
        .expect("failed to spawn vk-generation thread");

    match handle.join() {
        Ok(vk_hash) => {
            std::fs::write(snark_vk_hash_path, format!("{vk_hash:?}")).unwrap();
        }
        Err(e) => {
            std::panic::resume_unwind(e);
        }
    }
}

fn main() {
    // Rerun build script when `apps` or `Cargo.toml` get changed
    println!("cargo::rerun-if-changed=apps");
    println!("cargo::rerun-if-changed=Cargo.toml");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // `{manifest_dir}/../../setup.key`
    let setup_key_path = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("setup.key");
    let do_vk_generation = if setup_key_path.exists() {
        true
    } else {
        println!(
            "cargo::warning=`{}` not found; verification keys will not be generated",
            setup_key_path.display()
        );
        println!(
            "cargo::warning=to download run: `curl -L -o \"setup.key\" \"https://storage.googleapis.com/matterlabs-setup-keys-us/setup-keys/setup_2^24.key\"`"
        );
        false
    };

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

        let dir = manifest_dir.join("apps").join(&tag);
        std::fs::create_dir_all(&dir).expect("failed to create directory");
        for variant in [
            "multiblock_batch",
            "server_app",
            "server_app_logging_enabled",
        ] {
            let url = format!(
                "https://github.com/matter-labs/zksync-os/releases/download/{tag}/{variant}.bin"
            );
            let path = dir.join(format!("{variant}.bin"));
            if path.exists() {
                continue;
            }
            let resp = reqwest::blocking::get(url).expect("failed to download");
            let body = resp.bytes().expect("failed to read response body").to_vec();
            std::fs::write(path, body).expect("failed to write file");
        }

        let snake_case_version = tag.trim_start_matches("v").replace('.', "_");
        println!(
            "cargo:rustc-env=ZKSYNC_OS_{snake_case_version}_SOURCE_PATH={}",
            dir.to_string_lossy()
        );

        if do_vk_generation {
            generate_vk(&tag, setup_key_path.clone(), dir);
        }
    }
}
