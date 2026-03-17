use std::collections::HashMap;

use cargo_metadata::{MetadataCommand, PackageId};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use url::Url;

fn parse_git_tag(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, tag) = query_pairs
        .find(|(key, _)| key == "tag")
        .ok_or_else(|| anyhow::anyhow!("missing tag in git url `{url}`"))?;
    Ok(tag.to_string())
}

/// Proving version entry loaded from versions.toml.
struct ProvingEntry {
    forward_system_tag: String,
    app_bin_tag: String,
}

/// Load the `[proving.*]` sections from versions.toml.
///
/// Returns a map from forward_system_tag → (proving_version_name, app_bin_tag).
fn load_proving_versions() -> HashMap<String, (String, String)> {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let versions_path = workspace_root.join("versions.toml");
    println!("cargo:rerun-if-changed={}", versions_path.to_str().unwrap());

    let contents = std::fs::read_to_string(&versions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", versions_path.display()));

    let mut result = HashMap::new();
    let mut current_version: Option<String> = None;
    let mut current_entry = ProvingEntry {
        forward_system_tag: String::new(),
        app_bin_tag: String::new(),
    };

    let mut flush = |ver: &mut Option<String>, entry: &mut ProvingEntry| {
        if let Some(ver) = ver.take()
            && !entry.forward_system_tag.is_empty()
        {
            result.insert(
                std::mem::take(&mut entry.forward_system_tag),
                (ver, std::mem::take(&mut entry.app_bin_tag)),
            );
        }
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Detect [proving.VN] sections.
        if let Some(rest) = line.strip_prefix("[proving.") {
            flush(&mut current_version, &mut current_entry);
            current_version = rest.strip_suffix(']').map(String::from);
            current_entry = ProvingEntry {
                forward_system_tag: String::new(),
                app_bin_tag: String::new(),
            };
            continue;
        }

        // Any other section header flushes.
        if line.starts_with('[') {
            flush(&mut current_version, &mut current_entry);
            continue;
        }

        // Parse key = "value".
        if current_version.is_some()
            && let Some((key, value)) = parse_toml_string(line)
        {
            match key {
                "forward_system_tag" => current_entry.forward_system_tag = value,
                "app_bin_tag" => current_entry.app_bin_tag = value,
                _ => {}
            }
        }
    }

    // Flush last entry.
    flush(&mut current_version, &mut current_entry);

    result
}

fn parse_toml_string(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    let rest = rest.trim();
    let value = rest.strip_prefix('"')?.strip_suffix('"')?;
    Some((key, value.to_string()))
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

    // Load proving version mappings from versions.toml.
    let proving_versions = load_proving_versions();

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

        if let Some((proving_version, app_bin_tag)) = proving_versions.get(&tag) {
            // Use the app_bin_tag from versions.toml for downloading binaries.
            // This handles cases where the app.bin tag differs from the forward_system
            // tag (e.g. V6 where a toolchain change altered binaries).
            let download_tag = if app_bin_tag.is_empty() {
                &tag
            } else {
                app_bin_tag
            };

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
