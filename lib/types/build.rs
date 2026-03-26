/// Build script that reads `protocol-versions.toml` and generates
/// `protocol_config_generated.rs` with version lookup functions.
///
/// The generated file contains:
/// - `ForwardSystemVersion` enum with `TryFrom<u32>` and `Into<u32>`
/// - `forward_system_version_impl(minor, patch) -> Option<ForwardSystemVersion>`
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
    forward_system_version: u32,
    verifier_version: u32,
    vk_hash: String,
    app_bin_tag: Option<String>,
}

/// A `(crate_name, tag)` pair from a `[forward_system_version]` section.
struct CrateRef {
    crate_name: String,
    tag: String,
    /// Human-readable origin, e.g. `forward_system_version."v5".crate`.
    origin: String,
}

fn main() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let versions_path = workspace_root.join("protocol-versions.toml");
    let cargo_toml_path = workspace_root.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", versions_path.to_str().unwrap());
    println!(
        "cargo:rerun-if-changed={}",
        cargo_toml_path.to_str().unwrap()
    );

    let contents = std::fs::read_to_string(&versions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", versions_path.display()));

    let (entries, historical_vk_hashes, version_ids, crate_refs) = parse_toml(&contents);

    // Validate that every crate/tag referenced in protocol-versions.toml
    // exists as a workspace dependency in Cargo.toml with a matching git tag.
    let cargo_toml_contents = std::fs::read_to_string(&cargo_toml_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_toml_path.display()));
    validate_cargo_deps(&cargo_toml_contents, &crate_refs);

    let generated = generate_code(&entries, &historical_vk_hashes, &version_ids);

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

fn parse_toml(contents: &str) -> (Vec<ProtocolEntry>, Vec<String>, Vec<u32>, Vec<CrateRef>) {
    let root: Value = contents
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse protocol-versions.toml: {e}"));

    // Parse [forward_system_version."vN"] sections.
    let fs_versions = root
        .get("forward_system_version")
        .and_then(Value::as_table)
        .expect("missing [forward_system_version] sections");

    // Build a map from "vN" → app_bin_tag (if present), and collect all version IDs.
    // Also collect all (crate, tag) pairs for Cargo.toml validation.
    let mut fs_version_app_tags: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut version_ids: BTreeSet<u32> = BTreeSet::new();
    let mut crate_refs: Vec<CrateRef> = Vec::new();
    for (key, section) in fs_versions {
        let section = section
            .as_table()
            .unwrap_or_else(|| panic!("[forward_system_version.{key:?}] is not a table"));
        let app_bin_tag = section
            .get("app_bin_tag")
            .and_then(Value::as_str)
            .map(String::from);
        fs_version_app_tags.insert(key.clone(), app_bin_tag);
        version_ids.insert(parse_version_key(key));

        // Collect forward_system crate + tag.
        if let (Some(crate_name), Some(tag)) = (
            section.get("crate").and_then(Value::as_str),
            section.get("tag").and_then(Value::as_str),
        ) {
            crate_refs.push(CrateRef {
                crate_name: crate_name.to_string(),
                tag: tag.to_string(),
                origin: format!("forward_system_version.\"{key}\".crate"),
            });
        }
        // Collect simulation crate + tag (if present).
        if let (Some(crate_name), Some(tag)) = (
            section.get("simulation_crate").and_then(Value::as_str),
            section.get("simulation_tag").and_then(Value::as_str),
        ) {
            crate_refs.push(CrateRef {
                crate_name: crate_name.to_string(),
                tag: tag.to_string(),
                origin: format!("forward_system_version.\"{key}\".simulation_crate"),
            });
        }
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

        let fs_version_key = section
            .get("forward_system_version")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing forward_system_version for protocol {version_str}"));
        let forward_system_version = parse_version_key(fs_version_key);

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

        let app_bin_tag = fs_version_app_tags
            .get(fs_version_key)
            .unwrap_or_else(|| {
                panic!(
                    "protocol {version_str} references unknown forward_system_version {fs_version_key:?}"
                )
            })
            .clone();

        entries.push(ProtocolEntry {
            minor,
            patch,
            forward_system_version,
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

    (
        entries,
        historical_vk_hashes,
        version_ids.into_iter().collect(),
        crate_refs,
    )
}

/// Validates that every `(crate, tag)` pair from protocol-versions.toml exists
/// as a workspace dependency in Cargo.toml with the expected git tag.
/// Any mismatch is a compile-time error.
fn validate_cargo_deps(cargo_toml_contents: &str, crate_refs: &[CrateRef]) {
    let cargo_root: Value = cargo_toml_contents
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse workspace Cargo.toml: {e}"));

    let workspace_deps = cargo_root
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
        .unwrap_or_else(|| {
            // Flat layout: deps are at top level (no [workspace.dependencies]).
            cargo_root.as_table().expect("Cargo.toml is not a table")
        });

    let mut errors = Vec::new();
    for crate_ref in crate_refs {
        let dep = match workspace_deps.get(&crate_ref.crate_name) {
            Some(dep) => dep,
            None => {
                errors.push(format!(
                    "{}: crate '{}' not found in Cargo.toml",
                    crate_ref.origin, crate_ref.crate_name,
                ));
                continue;
            }
        };

        // Extract the git tag from the dependency value (which can be a table or inline table).
        let tag = dep.get("tag").and_then(Value::as_str).unwrap_or_else(|| {
            panic!(
                "Cargo.toml dependency '{}' has no 'tag' field",
                crate_ref.crate_name
            )
        });

        if tag != crate_ref.tag {
            errors.push(format!(
                "{}: crate '{}' has tag '{}' in protocol-versions.toml but '{}' in Cargo.toml",
                crate_ref.origin, crate_ref.crate_name, crate_ref.tag, tag,
            ));
        }
    }

    if !errors.is_empty() {
        let msg = errors.join("\n  ");
        panic!(
            "protocol-versions.toml / Cargo.toml mismatch:\n  {msg}\n\
             Please update protocol-versions.toml or Cargo.toml so they agree."
        );
    }
}

fn generate_code(
    entries: &[ProtocolEntry],
    historical_vk_hashes: &[String],
    version_ids: &[u32],
) -> String {
    let mut code = String::new();
    writeln!(
        code,
        "// Auto-generated from protocol-versions.toml — do not edit."
    )
    .unwrap();
    writeln!(code).unwrap();

    // ForwardSystemVersion enum
    writeln!(code, "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]").unwrap();
    writeln!(code, "#[repr(u32)]").unwrap();
    writeln!(code, "pub enum ForwardSystemVersion {{").unwrap();
    for &id in version_ids {
        writeln!(code, "    V{id} = {id},").unwrap();
    }
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // Display impl
    writeln!(code, "impl std::fmt::Display for ForwardSystemVersion {{").unwrap();
    writeln!(
        code,
        "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{"
    )
    .unwrap();
    writeln!(code, "        write!(f, \"v{{}}\", *self as u32)").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // TryFrom<u32>
    writeln!(code, "impl TryFrom<u32> for ForwardSystemVersion {{").unwrap();
    writeln!(code, "    type Error = u32;").unwrap();
    writeln!(
        code,
        "    fn try_from(value: u32) -> Result<Self, Self::Error> {{"
    )
    .unwrap();
    writeln!(code, "        match value {{").unwrap();
    for &id in version_ids {
        writeln!(code, "            {id} => Ok(Self::V{id}),").unwrap();
    }
    writeln!(code, "            _ => Err(value),").unwrap();
    writeln!(code, "        }}").unwrap();
    writeln!(code, "    }}").unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // From<ForwardSystemVersion> for u32
    writeln!(code, "impl From<ForwardSystemVersion> for u32 {{").unwrap();
    writeln!(
        code,
        "    fn from(v: ForwardSystemVersion) -> Self {{ v as u32 }}"
    )
    .unwrap();
    writeln!(code, "}}").unwrap();
    writeln!(code).unwrap();

    // forward_system_version_impl — returns ForwardSystemVersion
    writeln!(
        code,
        "fn forward_system_version_impl(minor: u64, patch: u64) -> Option<ForwardSystemVersion> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        writeln!(
            code,
            "        ({}, {}) => Some(ForwardSystemVersion::V{}),",
            e.minor, e.patch, e.forward_system_version
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
