use cargo_metadata::{MetadataCommand, PackageId};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::collections::HashMap;
use url::Url;

/// A single proving version entry from `versions.toml`.
#[derive(Deserialize)]
struct ProvingEntry {
    /// The forward_system git tag that identifies this proving version.
    forward_system_tag: String,
    /// The release tag from which app.bin files are downloaded.
    app_bin_tag: String,
}

/// Top-level structure of `versions.toml`.
#[derive(Deserialize)]
struct VersionsManifest {
    proving: HashMap<String, ProvingEntry>,
}

fn load_versions_manifest() -> anyhow::Result<VersionsManifest> {
    // versions.toml lives at the workspace root, two levels up from lib/multivm/
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("cannot determine workspace root from {manifest_dir}"))?;
    let path = workspace_root.join("versions.toml");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    println!("cargo:rerun-if-changed={}", path.display());
    toml::from_str(&content).map_err(|e| anyhow::anyhow!("failed to parse versions.toml: {e}"))
}

/// Look up the proving version for a given forward_system git tag.
fn proving_version_from_tag(manifest: &VersionsManifest, tag: &str) -> Option<String> {
    manifest
        .proving
        .iter()
        .find(|(_, entry)| entry.forward_system_tag == tag)
        .map(|(version, _)| version.clone())
}

fn parse_git_tag(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, tag) = query_pairs
        .find(|(key, _)| key == "tag")
        .ok_or_else(|| anyhow::anyhow!("missing tag in git url `{url}`"))?;
    Ok(tag.to_string())
}

const DOWNLOAD_MAX_ATTEMPTS: usize = 5;
const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const DOWNLOAD_BASE_BACKOFF_MS: u64 = 500;

fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

fn new_http_client() -> anyhow::Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("zksync-os-build-script/1.0"),
    );

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let bearer = format!("Bearer {}", token.trim());
        match HeaderValue::from_str(&bearer) {
            Ok(value) => {
                headers.insert(AUTHORIZATION, value);
            }
            Err(err) => {
                println!("cargo:warning=Ignoring invalid GITHUB_TOKEN format: {err}");
            }
        }
    }

    Ok(Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()?)
}

fn download_with_retry(client: &Client, url: &str, path: &str) -> anyhow::Result<()> {
    for attempt in 1..=DOWNLOAD_MAX_ATTEMPTS {
        let response = client.get(url).send();
        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let body = response.bytes()?;
                    std::fs::write(path, body.as_ref())?;
                    return Ok(());
                }

                if is_retryable_status(status) && attempt < DOWNLOAD_MAX_ATTEMPTS {
                    let delay_ms = DOWNLOAD_BASE_BACKOFF_MS * attempt as u64;
                    println!(
                        "cargo:warning=download attempt {attempt}/{DOWNLOAD_MAX_ATTEMPTS} failed with status {status} for {url}; retrying in {delay_ms}ms"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    continue;
                }

                anyhow::bail!("download failed with status {status} for {url}");
            }
            Err(err) => {
                if attempt < DOWNLOAD_MAX_ATTEMPTS {
                    let delay_ms = DOWNLOAD_BASE_BACKOFF_MS * attempt as u64;
                    println!(
                        "cargo:warning=download attempt {attempt}/{DOWNLOAD_MAX_ATTEMPTS} failed for {url}: {err}; retrying in {delay_ms}ms"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    continue;
                }

                anyhow::bail!("download request failed for {url}: {err}");
            }
        }
    }
    unreachable!("loop always returns on success or final attempt");
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let metadata = MetadataCommand::new().exec().unwrap();
    let client = new_http_client().expect("failed to create HTTP client");
    let versions = load_versions_manifest().expect("failed to load versions.toml");

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

        if let Some(proving_version) = proving_version_from_tag(&versions, &tag) {
            let entry = &versions.proving[&proving_version];
            let download_tag = &entry.app_bin_tag;

            let dir = format!("{manifest_dir}/apps/{download_tag}");
            std::fs::create_dir_all(&dir).expect("failed to create directory");
            for variant in [
                "multiblock_batch",
                "singleblock_batch",
                "singleblock_batch_logging_enabled",
            ] {
                let url = format!(
                    "https://github.com/matter-labs/zksync-os/releases/download/{download_tag}/{variant}.bin"
                );
                let path = format!("{dir}/{variant}.bin");
                if std::fs::exists(&path).expect("failed to check file existence") {
                    continue;
                }
                download_with_retry(&client, &url, &path).expect("failed to download");
            }

            println!("cargo:rustc-env=ZKSYNC_OS_{proving_version}_SOURCE_PATH={dir}");
            continue;
        }
    }
}
