/// Build script that reads `protocol-versions.toml` and generates
/// `protocol_config_generated.rs` with version lookup functions.
///
/// The generated file contains:
/// - `execution_version_impl(minor, patch) -> Option<u32>`
/// - `vk_hash_impl(minor, patch) -> Option<&'static str>`
/// - `proving_version_id_impl(minor, patch) -> Option<u32>`
/// - `ALL_KNOWN_VK_HASHES: &[&str]` — all non-zero VK hashes (current + historical)
use std::collections::BTreeSet;
use std::fmt::Write as FmtWrite;
use std::io::Write;

/// A parsed `[protocol."M.m.p"]` entry.
struct ProtocolEntry {
    minor: u64,
    patch: u64,
    execution_version: u32,
    proving_version_id: u32,
    vk_hash: String,
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

fn parse_toml(contents: &str) -> (Vec<ProtocolEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut historical_vk_hashes = Vec::new();

    enum Section {
        None,
        Protocol { minor: u64, patch: u64 },
        HistoricalVkHashes,
    }

    let mut section = Section::None;
    let mut cur_execution_version: Option<u32> = None;
    let mut cur_proving_version_id: Option<u32> = None;
    let mut cur_vk_hash: Option<String> = None;

    let flush = |section: &Section,
                 ev: &mut Option<u32>,
                 pv: &mut Option<u32>,
                 vk: &mut Option<String>,
                 entries: &mut Vec<ProtocolEntry>| {
        if let Section::Protocol { minor, patch } = section {
            if let (Some(execution_version), Some(proving_version_id), Some(vk_hash)) =
                (ev.take(), pv.take(), vk.take())
            {
                entries.push(ProtocolEntry {
                    minor: *minor,
                    patch: *patch,
                    execution_version,
                    proving_version_id,
                    vk_hash,
                });
            } else {
                panic!(
                    "incomplete protocol entry for {}.{}: missing execution_version, proving_version_id, or vk_hash",
                    minor, patch
                );
            }
        }
        *ev = None;
        *pv = None;
        *vk = None;
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Detect [protocol."M.m.p"] sections.
        if line.starts_with("[protocol.") {
            flush(
                &section,
                &mut cur_execution_version,
                &mut cur_proving_version_id,
                &mut cur_vk_hash,
                &mut entries,
            );
            // Parse version from [protocol."0.29.0"]
            let version_str = line
                .strip_prefix("[protocol.\"")
                .and_then(|s| s.strip_suffix("\"]"))
                .unwrap_or_else(|| panic!("malformed protocol section header: {line}"));
            let parts: Vec<&str> = version_str.split('.').collect();
            assert_eq!(parts.len(), 3, "expected M.m.p version in: {line}");
            let _major: u64 = parts[0].parse().unwrap();
            let minor: u64 = parts[1].parse().unwrap();
            let patch: u64 = parts[2].parse().unwrap();
            section = Section::Protocol { minor, patch };
            continue;
        }

        if line == "[historical_vk_hashes]" {
            flush(
                &section,
                &mut cur_execution_version,
                &mut cur_proving_version_id,
                &mut cur_vk_hash,
                &mut entries,
            );
            section = Section::HistoricalVkHashes;
            continue;
        }

        // Any other section header.
        if line.starts_with('[') {
            flush(
                &section,
                &mut cur_execution_version,
                &mut cur_proving_version_id,
                &mut cur_vk_hash,
                &mut entries,
            );
            section = Section::None;
            continue;
        }

        if let Some((key, value)) = parse_kv(line) {
            match &section {
                Section::Protocol { .. } => match key {
                    "execution_version" => {
                        cur_execution_version = Some(value.parse().unwrap());
                    }
                    "proving_version_id" => {
                        cur_proving_version_id = Some(value.parse().unwrap());
                    }
                    "vk_hash" => {
                        cur_vk_hash = Some(value);
                    }
                    _ => {} // ignore other keys (forward_system_crate, etc.)
                },
                Section::HistoricalVkHashes => {
                    // Values are VK hashes, keys are just labels.
                    historical_vk_hashes.push(value);
                }
                Section::None => {}
            }
        }
    }

    // Flush last entry.
    flush(
        &section,
        &mut cur_execution_version,
        &mut cur_proving_version_id,
        &mut cur_vk_hash,
        &mut entries,
    );

    (entries, historical_vk_hashes)
}

fn parse_kv(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    let rest = rest.trim();
    if let Some(value) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some((key, value.to_string()))
    } else {
        Some((key, rest.to_string()))
    }
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

    // proving_version_id_impl
    writeln!(
        code,
        "fn proving_version_id_impl(minor: u64, patch: u64) -> Option<u32> {{"
    )
    .unwrap();
    writeln!(code, "    match (minor, patch) {{").unwrap();
    for e in entries {
        writeln!(
            code,
            "        ({}, {}) => Some({}),",
            e.minor, e.patch, e.proving_version_id
        )
        .unwrap();
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

    code
}
