use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;

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

/// An app.bin download entry derived from protocol-versions.toml.
struct AppBinEntry {
    /// The tag to download app.bin from (may differ from forward_system_tag).
    app_bin_tag: String,
    /// Sanitized identifier for the env var (e.g., "V0_2_5" or "DEV_20260311").
    env_name: String,
}

/// Sanitize a tag string into a valid env var component.
/// e.g., "v0.2.5" → "V0_2_5", "dev-20260311" → "DEV_20260311"
fn sanitize_tag_for_env(tag: &str) -> String {
    tag.chars()
        .map(|c| match c {
            '.' | '-' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

/// Load protocol-versions.toml and extract app.bin download info.
///
/// Returns a map from forward_system_tag → AppBinEntry for protocol versions
/// that have an app_bin_tag set (i.e., those that need app.bin downloads).
fn load_app_bin_entries() -> HashMap<String, AppBinEntry> {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let versions_path = workspace_root.join("protocol-versions.toml");
    println!("cargo:rerun-if-changed={}", versions_path.to_str().unwrap());

    let contents = std::fs::read_to_string(&versions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", versions_path.display()));

    let mut result = HashMap::new();
    let mut in_protocol_section = false;
    let mut current_forward_system_tag = String::new();
    let mut current_app_bin_tag = String::new();

    let mut flush = |fs_tag: &mut String, abt: &mut String| {
        if !fs_tag.is_empty() && !abt.is_empty() {
            let env_name = sanitize_tag_for_env(abt);
            result.entry(std::mem::take(fs_tag)).or_insert(AppBinEntry {
                app_bin_tag: std::mem::take(abt),
                env_name,
            });
            // Clear both even if entry existed.
            *fs_tag = String::new();
            *abt = String::new();
        } else {
            fs_tag.clear();
            abt.clear();
        }
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Detect [protocol."M.m.p"] sections.
        if line.starts_with("[protocol.") {
            flush(&mut current_forward_system_tag, &mut current_app_bin_tag);
            in_protocol_section = true;
            continue;
        }

        // Any other section header.
        if line.starts_with('[') {
            flush(&mut current_forward_system_tag, &mut current_app_bin_tag);
            in_protocol_section = false;
            continue;
        }

        if in_protocol_section && let Some((key, value)) = parse_toml_string(line) {
            match key {
                "forward_system_tag" => current_forward_system_tag = value,
                "app_bin_tag" => current_app_bin_tag = value,
                _ => {}
            }
        }
    }

    // Flush last entry.
    flush(&mut current_forward_system_tag, &mut current_app_bin_tag);

    result
}

fn parse_toml_string(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    let rest = rest.trim();
    // Handle both string values ("...") and integer values.
    if let Some(value) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some((key, value.to_string()))
    } else {
        Some((key, rest.to_string()))
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

const APP_BIN_VARIANTS: &[&str] = &[
    "singleblock_batch",
    "multiblock_batch",
    "singleblock_batch_logging_enabled",
];

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let metadata = MetadataCommand::new().exec().unwrap();
    let client = new_http_client().expect("failed to create HTTP client");

    // Load app.bin download mappings from protocol-versions.toml.
    let app_bin_entries = load_app_bin_entries();

    // Collect unique app_bin_tag → env_name for code generation (ordered for determinism).
    let mut unique_tags: BTreeMap<String, String> = BTreeMap::new();

    // Find forward_system crates and download their app.bin files.
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

        if let Some(entry) = app_bin_entries.get(&tag) {
            let download_tag = &entry.app_bin_tag;
            let dir = format!("{manifest_dir}/apps/{download_tag}");
            std::fs::create_dir_all(&dir).expect("failed to create directory");
            for variant in APP_BIN_VARIANTS {
                let url = format!(
                    "https://github.com/matter-labs/zksync-os/releases/download/{download_tag}/{variant}.bin"
                );
                let path = format!("{dir}/{variant}.bin");
                if std::fs::exists(&path).expect("failed to check file existence") {
                    continue;
                }
                download_with_retry(&client, &url, &path).expect("failed to download");
            }

            println!(
                "cargo:rustc-env=ZKSYNC_OS_APPS_{}_SOURCE_PATH={dir}",
                entry.env_name
            );
            unique_tags
                .entry(entry.app_bin_tag.clone())
                .or_insert_with(|| entry.env_name.clone());
            continue;
        }
    }

    // Generate apps_generated.rs with include_bytes! and lookup function.
    generate_apps_code(&manifest_dir, &unique_tags);
}

fn generate_apps_code(manifest_dir: &str, tags: &BTreeMap<String, String>) {
    let mut code = String::new();
    writeln!(
        code,
        "// Auto-generated from protocol-versions.toml — do not edit."
    )
    .unwrap();
    writeln!(code).unwrap();

    // Generate const include_bytes! for each tag + variant.
    for (tag, env_name) in tags {
        for variant in APP_BIN_VARIANTS {
            let const_name = format!("{}_{}_BYTES", env_name, variant.to_ascii_uppercase());
            writeln!(
                code,
                "const {const_name}: &[u8] = include_bytes!(concat!(env!(\"ZKSYNC_OS_APPS_{env_name}_SOURCE_PATH\"), \"/{variant}.bin\"));"
            )
            .unwrap();
        }
        let _ = tag; // used only as the map key for ordering
        writeln!(code).unwrap();
    }

    // Generate lookup function.
    writeln!(
        code,
        "fn app_bin_bytes(tag: &str, variant: &str) -> Option<&'static [u8]> {{"
    )
    .unwrap();
    writeln!(code, "    match (tag, variant) {{").unwrap();
    for (tag, env_name) in tags {
        for variant in APP_BIN_VARIANTS {
            let const_name = format!("{}_{}_BYTES", env_name, variant.to_ascii_uppercase());
            writeln!(
                code,
                "        ({:?}, {:?}) => Some({const_name}),",
                tag, variant
            )
            .unwrap();
        }
    }
    writeln!(code, "        _ => None,").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_path = std::path::Path::new(&out_dir).join("apps_generated.rs");
    std::fs::write(&out_path, code).unwrap();

    // Also generate the old-style env var paths for backward compat during transition.
    // (The include_bytes! macros in the generated code use the env vars set above.)
    let _ = manifest_dir;
}
