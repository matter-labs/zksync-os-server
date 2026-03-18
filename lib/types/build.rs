/// Build script that reads `protocol-versions.toml` and generates
/// `protocol_config_generated.rs` with version lookup functions.
///
/// The generated file contains:
/// - `execution_version_impl(minor, patch) -> Option<u32>`
/// - `vk_hash_impl(minor, patch) -> Option<&'static str>`
/// - `verifier_version_impl(minor, patch) -> Option<u32>`
/// - `app_bin_tag_impl(minor, patch) -> Option<&'static str>`
/// - `ALL_KNOWN_VK_HASHES: &[&str]` — all non-zero VK hashes (current + historical)
use std::collections::BTreeSet;
use std::fmt::Write as FmtWrite;
use std::io::Write;
use toml::Value;

/// A resolved `[protocol."M.m.p"]` entry.
struct ProtocolEntry {
    minor: u64,
    patch: u64,
    execution_version: u32,
    verifier_version: u32,
    vk_hash: String,
    app_bin_tag: Option<String>,
}

fn main() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let versions_path = workspace_root.join("protocol-versions.toml");
    println!("cargo:rerun-if-changed={}", versions_path.to_str().unwrap());

    let contents = std::fs::read_to_string(&versions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", versions_path.display()));

    let (entries, historical_vk_hashes) = parse_toml(&contents);

    let generated = generate_code(&entries, &historical_vk_hashes);

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_path = std::path::Path::new(&out_dir).join("protocol_config_generated.rs");
    let mut file = std::fs::File::create(&out_path).unwrap();
    file.write_all(generated.as_bytes()).unwrap();
}

/// Parse a "vN" key into the integer N.
fn parse_version_key(key: &str) -> u32 {
    key.strip_prefix('v')
        .unwrap_or_else(|| panic!("expected 'vN' format, got: {key:?}"))
        .parse()
        .unwrap_or_else(|e| panic!("invalid version number in {key:?}: {e}"))
}

fn parse_toml(contents: &str) -> (Vec<ProtocolEntry>, Vec<String>) {
    let root: Value = contents
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse protocol-versions.toml: {e}"));

    // Parse [execution_version."vN"] sections.
    let exec_versions = root
        .get("execution_version")
        .and_then(Value::as_table)
        .expect("missing [execution_version] sections");

    // Build a map from "vN" → app_bin_tag (if present).
    let mut exec_version_app_tags: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for (key, section) in exec_versions {
        let section = section
            .as_table()
            .unwrap_or_else(|| panic!("[execution_version.{key:?}] is not a table"));
        let app_bin_tag = section
            .get("app_bin_tag")
            .and_then(Value::as_str)
            .map(String::from);
        exec_version_app_tags.insert(key.clone(), app_bin_tag);
    }

    // Parse [protocol."M.m.p"] sections.
    let protocols = root
        .get("protocol")
        .and_then(Value::as_table)
        .expect("missing [protocol] sections");

    let mut entries = Vec::new();
    for (version_str, section) in protocols {
        let parts: Vec<&str> = version_str.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected M.m.p version, got: {version_str:?}"
        );
        let _major: u64 = parts[0].parse().unwrap();
        let minor: u64 = parts[1].parse().unwrap();
        let patch: u64 = parts[2].parse().unwrap();

        let section = section
            .as_table()
            .unwrap_or_else(|| panic!("[protocol.{version_str:?}] is not a table"));

        let exec_version_key = section
            .get("execution_version")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing execution_version for protocol {version_str}"));
        let execution_version = parse_version_key(exec_version_key);

        let verifier_version = section
            .get("verifier_version_deprecated")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| {
                panic!("missing verifier_version_deprecated for protocol {version_str}")
            }) as u32;

        let vk_hash = section
            .get("vk_hash")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing vk_hash for protocol {version_str}"))
            .to_string();

        let app_bin_tag = exec_version_app_tags
            .get(exec_version_key)
            .unwrap_or_else(|| {
                panic!(
                    "protocol {version_str} references unknown execution_version {exec_version_key:?}"
                )
            })
            .clone();

        entries.push(ProtocolEntry {
            minor,
            patch,
            execution_version,
            verifier_version,
            vk_hash,
            app_bin_tag,
        });
    }

    // Parse [historical_vk_hashes].
    let mut historical_vk_hashes = Vec::new();
    if let Some(table) = root.get("historical_vk_hashes").and_then(Value::as_table) {
        for (_key, value) in table {
            if let Some(hash) = value.as_str() {
                historical_vk_hashes.push(hash.to_string());
            }
        }
    }

    (entries, historical_vk_hashes)
}

fn generate_code(entries: &[ProtocolEntry], historical_vk_hashes: &[String]) -> String {
    let mut code = String::new();
    writeln!(
        code,
        "// Auto-generated from protocol-versions.toml — do not edit."
    )
    .unwrap();
    writeln!(code).unwrap();

    // execution_version_impl
    writeln!(
        code,
        "fn execution_version_impl(minor: u64, patch: u64) -> Option<u32> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        writeln!(
            code,
            "        ({}, {}) => Some({}),",
            e.minor, e.patch, e.execution_version
        )
        .unwrap();
    }
    writeln!(code, "        _ => None,").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // vk_hash_impl
    writeln!(
        code,
        "fn vk_hash_impl(minor: u64, patch: u64) -> Option<&'static str> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        writeln!(
            code,
            "        ({}, {}) => Some({:?}),",
            e.minor, e.patch, e.vk_hash
        )
        .unwrap();
    }
    writeln!(code, "        _ => None,").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // verifier_version_impl
    writeln!(
        code,
        "fn verifier_version_impl(minor: u64, patch: u64) -> Option<u32> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        writeln!(
            code,
            "        ({}, {}) => Some({}),",
            e.minor, e.patch, e.verifier_version
        )
        .unwrap();
    }
    writeln!(code, "        _ => None,").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // app_bin_tag_impl
    writeln!(
        code,
        "fn app_bin_tag_impl(minor: u64, patch: u64) -> Option<&'static str> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        if let Some(tag) = &e.app_bin_tag {
            writeln!(
                code,
                "        ({}, {}) => Some({:?}),",
                e.minor, e.patch, tag
            )
            .unwrap();
        }
    }
    writeln!(code, "        _ => None,").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // Collect all unique non-zero VK hashes.
    let zero_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";
    let mut all_vk_hashes = BTreeSet::new();
    for e in entries {
        if e.vk_hash != zero_hash {
            all_vk_hashes.insert(e.vk_hash.clone());
        }
    }
    for h in historical_vk_hashes {
        all_vk_hashes.insert(h.clone());
    }

    writeln!(code, "const ALL_KNOWN_VK_HASHES: &[&str] = &[").unwrap();
    for h in &all_vk_hashes {
        writeln!(code, "    {:?},", h).unwrap();
    }
    writeln!(code, "];").unwrap();
    writeln!(code).unwrap();

    // List of all supported protocol versions for diagnostic messages.
    writeln!(code, "const ALL_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[").unwrap();
    for e in entries {
        writeln!(code, "    \"0.{}.{}\",", e.minor, e.patch).unwrap();
    }
    writeln!(code, "];").unwrap();

    code
}
