//! Load profiles: the single tuning surface for what `chaos load` sends.
//!
//! A profile is a TOML file naming the traffic shape (`tps`, `pattern`), the
//! relative weights of the tick-driven workloads, the cadence of the episodic
//! sagas, and a couple of size knobs. Built-in profiles ship compiled into the
//! binary (`--profile realistic`); a filesystem path works too
//! (`--profile ./my-mix.toml`). Command-line flags override whatever the
//! profile says.

use anyhow::Context as _;
use std::collections::BTreeMap;

/// Built-in profiles, resolvable by bare name. The files live in
/// `tools/chaos/profiles/` and double as documented starting points for
/// hand-rolled mixes.
const BUILT_IN: &[(&str, &str)] = &[
    ("default", include_str!("../../profiles/default.toml")),
    ("realistic", include_str!("../../profiles/realistic.toml")),
    ("guzzler", include_str!("../../profiles/guzzler.toml")),
    ("quiet", include_str!("../../profiles/quiet.toml")),
    ("smoke", include_str!("../../profiles/smoke.toml")),
];

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Combined rate of the tick-driven workloads while sending.
    pub tps: u32,
    /// `sustained` or `bursts`.
    #[serde(default = "default_pattern")]
    pub pattern: String,
    #[serde(default = "default_burst_secs")]
    pub burst_secs: u64,
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
    /// Relative weights per tick-driven workload; a workload absent here (or at
    /// weight 0) is off. Names must match the workload registry.
    pub weights: BTreeMap<String, u32>,
    #[serde(default)]
    pub sagas: SagaConfig,
    #[serde(default)]
    pub knobs: Knobs,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SagaConfig {
    /// Seconds between nonce-race episodes; absent = saga off.
    pub nonce_race_secs: Option<u64>,
    /// Seconds between deposit episodes (every fifth is a burst); absent = off.
    pub deposits_secs: Option<u64>,
    /// Seconds between withdrawal-pipeline ticks; absent = off.
    pub withdrawals_secs: Option<u64>,
    /// Seconds between failed-deposit episodes; absent = off.
    pub failed_deposits_secs: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Knobs {
    /// Gas limit for each gas-guzzler transaction (it burns almost all of it).
    #[serde(default = "default_guzzler_gas")]
    pub guzzler_gas: u64,
    /// Calldata size of each blob transaction, in KiB.
    #[serde(default = "default_blob_kib")]
    pub blob_kib: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            guzzler_gas: default_guzzler_gas(),
            blob_kib: default_blob_kib(),
        }
    }
}

fn default_pattern() -> String {
    "sustained".to_string()
}
fn default_burst_secs() -> u64 {
    5
}
fn default_idle_secs() -> u64 {
    15
}
fn default_guzzler_gas() -> u64 {
    3_000_000
}
fn default_blob_kib() -> u64 {
    48
}

/// Resolves `--profile`: a built-in name first, a filesystem path second.
pub fn resolve(spec: &str) -> anyhow::Result<Profile> {
    let text = match BUILT_IN.iter().find(|(name, _)| *name == spec) {
        Some((_, text)) => (*text).to_string(),
        None => std::fs::read_to_string(spec).with_context(|| {
            let names: Vec<&str> = BUILT_IN.iter().map(|(name, _)| *name).collect();
            format!(
                "profile {spec:?} is neither a built-in ({}) nor a readable file",
                names.join(", ")
            )
        })?,
    };
    let profile: Profile =
        toml::from_str(&text).with_context(|| format!("parsing profile {spec:?}"))?;
    anyhow::ensure!(profile.tps > 0, "profile {spec:?} has tps 0");
    anyhow::ensure!(
        matches!(profile.pattern.as_str(), "sustained" | "bursts"),
        "profile {spec:?}: pattern must be `sustained` or `bursts`",
    );
    anyhow::ensure!(
        profile.weights.values().any(|weight| *weight > 0),
        "profile {spec:?} enables no workloads",
    );
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_profile_parses() {
        for (name, _) in BUILT_IN {
            let profile = resolve(name).unwrap_or_else(|err| panic!("profile {name}: {err}"));
            assert!(profile.tps > 0);
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = "tps = 1\ntyop = 3\n[weights]\ntransfers = 1\n";
        assert!(toml::from_str::<Profile>(text).is_err());
    }

    #[test]
    fn unknown_profile_name_names_the_built_ins() {
        let err = resolve("no-such-profile").unwrap_err().to_string();
        assert!(err.contains("realistic"), "unhelpful error: {err}");
    }
}
