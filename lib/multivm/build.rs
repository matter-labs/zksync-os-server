use cargo_metadata::{MetadataCommand, PackageId};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use url::Url;

fn parse_git_ref(package_id: &PackageId, key: &str) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, value) = query_pairs
        .find(|(k, _)| k == key)
        .ok_or_else(|| anyhow::anyhow!("missing {key} in git url `{url}`"))?;
    Ok(value.to_string())
}

/// Returns (proving_version, download_tag) for a given git tag.
fn proving_version_from_tag(tag: &str) -> Option<(&'static str, &'static str)> {
    match tag {
        "v0.2.9-interface-v0.0.15" => Some(("V6", "v0.2.5")),
        "v0.3.0" => Some(("V7", "dev-20260421")),
        _ => None,
    }
}

/// Returns (proving_version, download_tag) for a given git branch.
fn proving_version_from_branch(branch: &str) -> Option<(&'static str, &'static str)> {
    match branch {
        "v0.2.x" => Some(("V6", "v0.2.5")),
        "v0.3.x" => Some(("V7", "dev-20260421")),
        _ => None,
    }
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

    // Find forward_system crate and expose its path to the directory containing `app*.bin` files.
    for package in &metadata.packages {
        if package.name.as_str() != "forward_system" {
            continue;
        }

        let version_and_tag = if let Ok(tag) = parse_git_ref(&package.id, "tag") {
            proving_version_from_tag(&tag)
        } else if let Ok(branch) = parse_git_ref(&package.id, "branch") {
            proving_version_from_branch(&branch)
        } else {
            continue;
        };

        let Some((proving_version, download_tag)) = version_and_tag else {
            continue;
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
    }
}
