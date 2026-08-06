use cargo_metadata::{MetadataCommand, PackageId};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use url::Url;

struct BinarySourceConfig {
    proving_version: &'static str,
    download_tag: &'static str,
}

fn parse_git_reference(package_id: &PackageId) -> anyhow::Result<String> {
    let url = Url::parse(&package_id.to_string())?;
    let mut query_pairs = url.query_pairs();
    let (_, reference) = query_pairs
        .find(|(key, _)| key == "tag" || key == "branch" || key == "rev")
        .ok_or_else(|| anyhow::anyhow!("missing tag, branch or rev in git url `{url}`"))?;
    Ok(reference.to_string())
}

// The `*-interface-v0.1.3-2026-02-10` tags are toolchain rebuilds without published app
// binaries; binaries are downloaded from the original releases they rebuild.
fn binary_source_config(reference: &str) -> Option<BinarySourceConfig> {
    match reference {
        "v0.3.1-interface-v0.1.3-2026-02-10" => Some(BinarySourceConfig {
            proving_version: "V7",
            download_tag: "v0.3.1-interface-v0.1.3",
        }),
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
        let Ok(reference) = parse_git_reference(&package.id) else {
            continue;
        };

        if let Some(config) = binary_source_config(&reference) {
            let dir = format!("{manifest_dir}/apps/{}", config.download_tag);
            std::fs::create_dir_all(&dir).expect("failed to create directory");
            for variant in [
                "multiblock_batch",
                "singleblock_batch",
                "singleblock_batch_logging_enabled",
            ] {
                let url = format!(
                    "https://github.com/matter-labs/zksync-os/releases/download/{}/{variant}.bin",
                    config.download_tag
                );
                let path = format!("{dir}/{variant}.bin");
                if std::fs::exists(&path).expect("failed to check file existence") {
                    continue;
                }
                download_with_retry(&client, &url, &path).expect("failed to download");
            }

            println!(
                "cargo:rustc-env=ZKSYNC_OS_{}_SOURCE_PATH={dir}",
                config.proving_version
            );
            continue;
        }
    }
}
