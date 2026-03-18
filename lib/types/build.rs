/// Build script that reads `protocol-versions.toml` and generates
/// `protocol_config_generated.rs` with version lookup functions.
///
/// The generated file contains:
/// - `execution_version_impl(minor, patch) -> Option<u32>`
/// - `vk_hash_impl(minor, patch) -> Option<&'static str>`
/// - `proving_version_id_impl(minor, patch) -> Option<u32>`
/// - `app_bin_tag_impl(minor, patch) -> Option<&'static str>`
/// - `ALL_KNOWN_VK_HASHES: &[&str]` — all non-zero VK hashes (current + historical)
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as FmtWrite;
use std::io::Write;

/// A parsed `[execution_version."Vn"]` entry.
struct ExecutionVersionDef {
    id: u32,
}

/// A parsed `[proving_version."Vn"]` entry.
struct ProvingVersionDef {
    id: u32,
    vk_hash: String,
}

/// A resolved `[protocol."M.m.p"]` entry.
struct ProtocolEntry {
    minor: u64,
    patch: u64,
    execution_version: u32,
    proving_version_id: u32,
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

fn parse_toml(contents: &str) -> (Vec<ProtocolEntry>, Vec<String>) {
    let mut exec_versions: HashMap<String, ExecutionVersionDef> = HashMap::new();
    let mut proving_versions: HashMap<String, ProvingVersionDef> = HashMap::new();
    let mut historical_vk_hashes = Vec::new();

    // Raw protocol entries before resolution.
    struct RawProtocol {
        minor: u64,
        patch: u64,
        exec_ref: String,
        proving_ref: String,
        app_bin_tag: Option<String>,
    }
    let mut raw_protocols = Vec::new();

    enum Section {
        None,
        ExecutionVersion(String),
        ProvingVersion(String),
        Protocol { minor: u64, patch: u64 },
        HistoricalVkHashes,
    }

    let mut section = Section::None;

    // Temporaries for current section.
    let mut cur_id: Option<u32> = None;
    let mut cur_vk_hash: Option<String> = None;
    let mut cur_exec_ref: Option<String> = None;
    let mut cur_proving_ref: Option<String> = None;
    let mut cur_app_bin_tag: Option<String> = None;

    let flush = |section: &Section,
                 id: &mut Option<u32>,
                 vk: &mut Option<String>,
                 exec_ref: &mut Option<String>,
                 proving_ref: &mut Option<String>,
                 abt: &mut Option<String>,
                 exec_versions: &mut HashMap<String, ExecutionVersionDef>,
                 proving_versions: &mut HashMap<String, ProvingVersionDef>,
                 raw_protocols: &mut Vec<RawProtocol>| {
        match section {
            Section::ExecutionVersion(name) => {
                let id_val = id
                    .take()
                    .unwrap_or_else(|| panic!("missing id for execution_version.{name}"));
                exec_versions.insert(name.clone(), ExecutionVersionDef { id: id_val });
            }
            Section::ProvingVersion(name) => {
                let id_val = id
                    .take()
                    .unwrap_or_else(|| panic!("missing id for proving_version.{name}"));
                let vk_hash = vk
                    .take()
                    .unwrap_or_else(|| panic!("missing vk_hash for proving_version.{name}"));
                proving_versions.insert(
                    name.clone(),
                    ProvingVersionDef {
                        id: id_val,
                        vk_hash,
                    },
                );
            }
            Section::Protocol { minor, patch } => {
                let er = exec_ref.take().unwrap_or_else(|| {
                    panic!("missing execution_version for protocol {minor}.{patch}")
                });
                let pr = proving_ref.take().unwrap_or_else(|| {
                    panic!("missing proving_version for protocol {minor}.{patch}")
                });
                raw_protocols.push(RawProtocol {
                    minor: *minor,
                    patch: *patch,
                    exec_ref: er,
                    proving_ref: pr,
                    app_bin_tag: abt.take(),
                });
            }
            _ => {}
        }
        *id = None;
        *vk = None;
        *exec_ref = None;
        *proving_ref = None;
        *abt = None;
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Detect section headers.
        if line.starts_with('[') {
            flush(
                &section,
                &mut cur_id,
                &mut cur_vk_hash,
                &mut cur_exec_ref,
                &mut cur_proving_ref,
                &mut cur_app_bin_tag,
                &mut exec_versions,
                &mut proving_versions,
                &mut raw_protocols,
            );

            if let Some(name) = line
                .strip_prefix("[execution_version.\"")
                .and_then(|s| s.strip_suffix("\"]"))
            {
                section = Section::ExecutionVersion(name.to_string());
            } else if let Some(name) = line
                .strip_prefix("[proving_version.\"")
                .and_then(|s| s.strip_suffix("\"]"))
            {
                section = Section::ProvingVersion(name.to_string());
            } else if let Some(version_str) = line
                .strip_prefix("[protocol.\"")
                .and_then(|s| s.strip_suffix("\"]"))
            {
                let parts: Vec<&str> = version_str.split('.').collect();
                assert_eq!(parts.len(), 3, "expected M.m.p version in: {line}");
                let _major: u64 = parts[0].parse().unwrap();
                let minor: u64 = parts[1].parse().unwrap();
                let patch: u64 = parts[2].parse().unwrap();
                section = Section::Protocol { minor, patch };
            } else if line == "[historical_vk_hashes]" {
                section = Section::HistoricalVkHashes;
            } else {
                section = Section::None;
            }
            continue;
        }

        if let Some((key, value)) = parse_kv(line) {
            match &section {
                Section::ExecutionVersion(_) => {
                    if key == "id" {
                        cur_id = Some(value.parse().unwrap());
                    }
                }
                Section::ProvingVersion(_) => match key {
                    "id" => cur_id = Some(value.parse().unwrap()),
                    "vk_hash" => cur_vk_hash = Some(value),
                    _ => {}
                },
                Section::Protocol { .. } => match key {
                    "execution_version" => cur_exec_ref = Some(value),
                    "proving_version" => cur_proving_ref = Some(value),
                    "app_bin_tag" => cur_app_bin_tag = Some(value),
                    _ => {}
                },
                Section::HistoricalVkHashes => {
                    historical_vk_hashes.push(value);
                }
                Section::None => {}
            }
        }
    }

    // Flush last entry.
    flush(
        &section,
        &mut cur_id,
        &mut cur_vk_hash,
        &mut cur_exec_ref,
        &mut cur_proving_ref,
        &mut cur_app_bin_tag,
        &mut exec_versions,
        &mut proving_versions,
        &mut raw_protocols,
    );

    // Resolve references.
    let entries = raw_protocols
        .into_iter()
        .map(|rp| {
            let ev = exec_versions.get(&rp.exec_ref).unwrap_or_else(|| {
                panic!(
                    "protocol {}.{} references unknown execution_version {:?}",
                    rp.minor, rp.patch, rp.exec_ref
                )
            });
            let pv = proving_versions.get(&rp.proving_ref).unwrap_or_else(|| {
                panic!(
                    "protocol {}.{} references unknown proving_version {:?}",
                    rp.minor, rp.patch, rp.proving_ref
                )
            });
            ProtocolEntry {
                minor: rp.minor,
                patch: rp.patch,
                execution_version: ev.id,
                proving_version_id: pv.id,
                vk_hash: pv.vk_hash.clone(),
                app_bin_tag: rp.app_bin_tag,
            }
        })
        .collect();

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

    code
}
