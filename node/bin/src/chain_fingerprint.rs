//! The committee-uniform configuration and resolved chain / consensus-era
//! identity, canonically hashed.
//!
//! N operators must hold one truth: the committee schedule, the chain
//! constants verification pins, the epoch geometry, the consensus timing,
//! and the actual anchor-derived era. Today drift is discovered by its
//! symptoms — a validator stalling at an epoch boundary, certificates
//! failing to verify, false byzantine alarms — each with an expensive remedy.
//! This fingerprint turns drift into an earlier warning instead: every node
//! logs it at startup and serves it in
//! `/status.consensus.chain_fingerprint`; anything comparing two nodes (the
//! chaos rig's watcher does, dashboards can) alarms on mismatch.
//!
//! What goes in: everything that must be identical across the committee.
//! What stays out: per-node facts (private keys, local listen addresses,
//! paths, role, batcher/prover wiring). Adding a committee-uniform config
//! field? Add it here — a field that verification, admission, or rotation
//! depends on and that is *not* fingerprinted is a drift detector with a
//! blind spot. This remains a diagnostic signal only; p2p and signing
//! namespaces define protocol compatibility separately.

use crate::config::Config;
use crate::consensus::ObserverPeer;
use alloy::hex;
use commonware_codec::Encode as _;
use sha2::{Digest as _, Sha256};
use std::net::SocketAddr;

/// Canonical observer identity: the key and address arrive here already
/// decoded from their configured spellings, and the list is sorted — order
/// carries no meaning for admission.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
struct ObserverSurface {
    network_key: Vec<u8>,
    address: SocketAddr,
}

/// The fingerprinted surface. Serialization is canonical because the struct's
/// field order is fixed and every value is a plain scalar or an ordered list;
/// semantically identical surfaces therefore produce identical input bytes.
#[derive(serde::Serialize)]
struct ChainSurface {
    l2_chain_id: Option<u64>,
    genesis_height: u64,
    // Full digest of `ConsensusBlock::genesis_at(anchor height, local anchor
    // hash)`: equal config over different local anchor blocks must diagnose as
    // different eras.
    consensus_era: [u8; 32],
    // The signing/handshake namespace: two versions cannot even pair.
    consensus_protocol_version: u32,
    // Epoch geometry and consensus timing: rotation and view pacing.
    epoch_length: u64,
    leader_timeout_nanos: u128,
    certification_timeout_nanos: u128,
    idle_heartbeat_nanos: u128,
    max_timestamp_skew_nanos: u128,
    max_message_size: u32,
    // Observer order is not semantic; identities and advertised addresses are.
    observers: Vec<ObserverSurface>,
    // The committee schedule, normalized: the `validators` shorthand and an
    // explicit `committees` list with one epoch-0 entry are the same schedule.
    schedule: Vec<(u64, Vec<String>)>,
    // The on-chain validator registry: whether/how it participates, where it
    // lives, when it takes over, and the lookahead rule — all facts every
    // committee member must agree on before a rotation depends on them.
    registry_mode: String,
    registry_address: Option<[u8; 20]>,
    registry_flip_epoch: Option<u64>,
    registry_lookahead_epochs: u64,
    // Chain constants that verification pins (a proposal violating any of
    // these is rejected, so a drifted node false-alarms as byzantine).
    fee_collector: [u8; 20],
    block_gas_limit: u64,
    block_pubdata_limit_bytes: u64,
    max_transactions_in_block: usize,
    interop_roots_per_block: u64,
    block_time_nanos: u128,
    // Exactly the fee-rule inputs consumed by proposal verification. Oracle
    // inputs stay out: validators bound movement from the parent and need not
    // observe the same off-chain price at the same instant.
    fee_base_fee_override: Option<u128>,
    fee_native_per_gas: u64,
    fee_pubdata_price_override: Option<u128>,
    fee_pubdata_price_cap: Option<u128>,
    fee_native_price_override: Option<u128>,
}

fn normalized_observers(observers: &[ObserverPeer]) -> Vec<ObserverSurface> {
    let mut observers: Vec<_> = observers
        .iter()
        .map(|observer| ObserverSurface {
            network_key: observer.network_key.encode().to_vec(),
            address: observer.address,
        })
        .collect();
    observers.sort_unstable();
    observers
}

/// Canonical hex diagnostic fingerprint of the committee-uniform config plus
/// the anchor-derived consensus era and resolved observer admission set.
pub fn chain_fingerprint(
    config: &Config,
    consensus_era: [u8; 32],
    observers: &[ObserverPeer],
) -> String {
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
    let observers = normalized_observers(observers);
    let surface = ChainSurface {
        l2_chain_id: config.genesis_config.chain_id,
        genesis_height: consensus.genesis_height,
        consensus_era,
        consensus_protocol_version: consensus.protocol_version,
        epoch_length: consensus.epoch_length,
        leader_timeout_nanos: consensus.leader_timeout.as_nanos(),
        certification_timeout_nanos: consensus.certification_timeout.as_nanos(),
        idle_heartbeat_nanos: consensus.idle_heartbeat.as_nanos(),
        max_timestamp_skew_nanos: consensus.max_timestamp_skew.as_nanos(),
        max_message_size: consensus.max_message_size.get(),
        observers,
        schedule,
        registry_mode: consensus.registry_mode.as_str().to_string(),
        registry_address: consensus
            .registry_address
            .map(|address| address.into_array()),
        registry_flip_epoch,
        registry_lookahead_epochs: zksync_os_consensus_core::LOOKAHEAD_EPOCHS,
        fee_collector: config.sequencer_config.fee_collector_address.into_array(),
        block_gas_limit: config.sequencer_config.block_gas_limit,
        block_pubdata_limit_bytes: config.sequencer_config.block_pubdata_limit_bytes,
        max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
        interop_roots_per_block: config.sequencer_config.interop_roots_per_block,
        block_time_nanos: config.sequencer_config.block_time.as_nanos(),
        fee_base_fee_override: config
            .fee_config
            .base_fee_override
            .as_ref()
            .map(|value| value.to::<u128>()),
        fee_native_per_gas: config.fee_config.native_per_gas,
        fee_pubdata_price_override: config
            .fee_config
            .pubdata_price_override
            .as_ref()
            .map(|value| value.to::<u128>()),
        fee_pubdata_price_cap: config
            .fee_config
            .pubdata_price_cap
            .as_ref()
            .map(|value| value.to::<u128>()),
        fee_native_price_override: config
            .fee_config
            .native_price_override
            .as_ref()
            .map(|value| value.to::<u128>()),
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
    fn surface_serialization_is_stable_and_identity_sensitive() {
        let surface = |epoch_length: u64, era_byte: u8| super::ChainSurface {
            l2_chain_id: Some(270),
            genesis_height: 0,
            consensus_era: [era_byte; 32],
            consensus_protocol_version: 1,
            epoch_length,
            leader_timeout_nanos: 1_000_000_000,
            certification_timeout_nanos: 2_000_000_000,
            idle_heartbeat_nanos: 600_000_000_000,
            max_timestamp_skew_nanos: 10_000_000_001,
            max_message_size: 16 * 1024 * 1024,
            observers: vec![super::ObserverSurface {
                network_key: vec![1; 32],
                address: "127.0.0.1:3054".parse().unwrap(),
            }],
            schedule: vec![(0, vec!["a@1:1".into(), "b@2:2".into()])],
            registry_mode: "schedule".into(),
            registry_address: None,
            registry_flip_epoch: None,
            registry_lookahead_epochs: 2,
            fee_collector: [0; 20],
            block_gas_limit: 1,
            block_pubdata_limit_bytes: 2,
            max_transactions_in_block: 3,
            interop_roots_per_block: 4,
            block_time_nanos: 250_000_000,
            fee_base_fee_override: Some(5),
            fee_native_per_gas: 100,
            fee_pubdata_price_override: Some(6),
            fee_pubdata_price_cap: Some(7),
            fee_native_price_override: Some(8),
        };
        let hash = |s: &super::ChainSurface| {
            use sha2::Digest as _;
            alloy::hex::encode(&sha2::Sha256::digest(serde_json::to_vec(s).unwrap())[..8])
        };
        assert_eq!(
            hash(&surface(100, 9)),
            hash(&surface(100, 9)),
            "deterministic"
        );
        assert_ne!(
            hash(&surface(100, 9)),
            hash(&surface(200, 9)),
            "uniform facts move the fingerprint"
        );
        assert_ne!(
            hash(&surface(100, 9)),
            hash(&surface(100, 10)),
            "the resolved consensus era moves the fingerprint"
        );
    }

    #[test]
    fn observer_admission_is_canonicalized_as_a_set() {
        use commonware_codec::DecodeExt as _;
        use commonware_cryptography::{Signer as _, ed25519};

        let observer = |seed: u8, port: u16| crate::consensus::ObserverPeer {
            network_key: ed25519::PrivateKey::decode([seed; 32].as_slice())
                .expect("seed")
                .public_key(),
            address: format!("127.0.0.1:{port}").parse().expect("address"),
        };
        let a = observer(1, 3054);
        let b = observer(2, 3055);
        assert_eq!(
            super::normalized_observers(&[a.clone(), b.clone()]),
            super::normalized_observers(&[b, a])
        );
    }
}
