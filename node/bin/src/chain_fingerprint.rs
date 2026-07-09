//! The committee-uniform configuration surface, canonically hashed.
//!
//! N operators must hold one truth: the committee schedule, the chain
//! constants verification pins, the epoch geometry, the consensus timing.
//! Today drift is discovered by its symptoms — a validator stalling at an
//! epoch boundary, certificates failing to verify, false byzantine alarms —
//! each with an expensive remedy. This fingerprint turns drift into a
//! *pre-boundary* warning instead: every node logs it at startup and serves
//! it in `/status.consensus.chain_fingerprint`; anything comparing two nodes
//! (the chaos rig's watcher does, dashboards can) alarms on mismatch.
//!
//! What goes in: everything that must be identical across the committee.
//! What stays out: per-node facts (keys, addresses, ports, paths, role,
//! batcher/prover wiring). Adding a committee-uniform config field? Add it
//! here — a field that verification or rotation depends on and that is *not*
//! fingerprinted is a drift detector with a blind spot.

use crate::config::Config;
use alloy::hex;
use sha2::{Digest as _, Sha256};

/// The fingerprinted surface. Serialization is canonical because the struct's
/// field order is fixed and every value is a plain scalar or an ordered list —
/// two configs agree on this surface if and only if their fingerprints match.
#[derive(serde::Serialize)]
struct ChainSurface {
    l2_chain_id: Option<u64>,
    genesis_height: u64,
    // The signing/handshake namespace: two versions cannot even pair.
    consensus_protocol_version: u32,
    // Epoch geometry and consensus timing: rotation and view pacing.
    epoch_length: u64,
    leader_timeout_ms: u128,
    certification_timeout_ms: u128,
    idle_heartbeat_ms: u128,
    // The committee schedule, normalized: the `validators` shorthand and an
    // explicit `committees` list with one epoch-0 entry are the same schedule.
    schedule: Vec<(u64, Vec<String>)>,
    // The on-chain validator registry: whether/how it participates, where it
    // lives, when it takes over, and the lookahead rule — all facts every
    // committee member must agree on before a rotation depends on them.
    registry_mode: String,
    registry_address: Option<String>,
    registry_flip_epoch: Option<u64>,
    registry_lookahead_epochs: u64,
    // Chain constants that verification pins (a proposal violating any of
    // these is rejected, so a drifted node false-alarms as byzantine).
    fee_collector: String,
    block_gas_limit: u64,
    block_pubdata_limit_bytes: u64,
    max_transactions_in_block: usize,
    interop_roots_per_block: u64,
    block_time_ms: u128,
}

/// Canonical hex fingerprint of `config`'s committee-uniform surface.
pub fn chain_fingerprint(config: &Config) -> String {
    let consensus = &config.consensus_config;
    let schedule = if consensus.committees.is_empty() {
        vec![(0, consensus.validators.clone())]
    } else {
        consensus
            .committees
            .iter()
            .map(|entry| (entry.activation_epoch, entry.validators.clone()))
            .collect()
    };
    let registry_flip_epoch = consensus
        .committees
        .iter()
        .find(|entry| entry.source == crate::config::CommitteeEntrySource::Registry)
        .map(|entry| entry.activation_epoch);
    let surface = ChainSurface {
        l2_chain_id: config.genesis_config.chain_id,
        genesis_height: consensus.genesis_height,
        consensus_protocol_version: consensus.protocol_version,
        epoch_length: consensus.epoch_length,
        leader_timeout_ms: consensus.leader_timeout.as_millis(),
        certification_timeout_ms: consensus.certification_timeout.as_millis(),
        idle_heartbeat_ms: consensus.idle_heartbeat.as_millis(),
        schedule,
        registry_mode: consensus.registry_mode.as_str().to_string(),
        registry_address: consensus
            .registry_address
            .map(|address| format!("{address:?}")),
        registry_flip_epoch,
        registry_lookahead_epochs: zksync_os_consensus_core::LOOKAHEAD_EPOCHS,
        fee_collector: format!("{:?}", config.sequencer_config.fee_collector_address),
        block_gas_limit: config.sequencer_config.block_gas_limit,
        block_pubdata_limit_bytes: config.sequencer_config.block_pubdata_limit_bytes,
        max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
        interop_roots_per_block: config.sequencer_config.interop_roots_per_block,
        block_time_ms: config.sequencer_config.block_time.as_millis(),
    };
    let canonical = serde_json::to_vec(&surface).expect("plain data serializes");
    let digest = Sha256::digest(&canonical);
    hex::encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    /// The fingerprint moves with committee-uniform facts and ignores per-node
    /// ones — pinned lightly here through the surface struct itself (full
    /// config construction needs a chain fixture; the L3s cover the wired
    /// path by comparing live /status values across a committee).
    #[test]
    fn surface_serialization_is_stable_and_order_sensitive() {
        let surface = |epoch_length: u64| super::ChainSurface {
            l2_chain_id: Some(270),
            genesis_height: 0,
            consensus_protocol_version: 1,
            epoch_length,
            leader_timeout_ms: 1000,
            certification_timeout_ms: 2000,
            idle_heartbeat_ms: 600_000,
            schedule: vec![(0, vec!["a@1:1".into(), "b@2:2".into()])],
            registry_mode: "schedule".into(),
            registry_address: None,
            registry_flip_epoch: None,
            registry_lookahead_epochs: 2,
            fee_collector: "0x00".into(),
            block_gas_limit: 1,
            block_pubdata_limit_bytes: 2,
            max_transactions_in_block: 3,
            interop_roots_per_block: 4,
            block_time_ms: 250,
        };
        let hash = |s: &super::ChainSurface| {
            use sha2::Digest as _;
            alloy::hex::encode(&sha2::Sha256::digest(serde_json::to_vec(s).unwrap())[..8])
        };
        assert_eq!(hash(&surface(100)), hash(&surface(100)), "deterministic");
        assert_ne!(
            hash(&surface(100)),
            hash(&surface(200)),
            "uniform facts move the fingerprint"
        );
    }
}
