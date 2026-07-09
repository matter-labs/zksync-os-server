//! Runs this node as one validator of the BFT committee.
//!
//! The consensus world lives on its own OS thread with its own async runtime and its
//! own networking stack, deliberately isolated from the node's main runtime: consensus
//! must keep making progress (or failing loudly) independently of RPC load or pipeline
//! stalls. The two worlds touch in exactly four places:
//!
//! - the execution environment (given to consensus at spawn), through which consensus
//!   builds, verifies, and commits blocks;
//! - the committed-payload channel, feeding finalized blocks into the node's
//!   persistence pipeline;
//! - the L2 mempool, whose transactions the committee gossips among itself so a
//!   transaction reaches the next leader no matter which validator's RPC received it;
//! - a death signal back to the node — if consensus dies, the node must go down with
//!   it rather than keep serving a chain that stopped.

use alloy::consensus::transaction::SignerRecoverable as _;
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use anyhow::Context as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519;
use commonware_p2p::authenticated::lookup;
use commonware_p2p::{Address, AddressableManager as _, Ingress, Receiver, Sender};
use commonware_runtime::{Metrics as _, Quota, Runner as _, Spawner as _, Supervisor as _};
use commonware_utils::TryCollect as _;
use commonware_utils::ordered::{BiMap, Map};
use commonware_utils::union_unique;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use zksync_os_consensus_core::types::{
    Activity, Attributable as _, ConsensusActivity, SchemeProvider,
};
use zksync_os_consensus_core::{
    Channels, CommitteeSchedule, CommitteeSource, ScheduleEntry, StackConfig, start_validator,
};
use zksync_os_consensus_execution::NodeExecutionEnv;
use zksync_os_consensus_execution::metrics::CONSENSUS_METRICS;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_status_server::{ConsensusMetricsEncoder, FinalizedObservation, RegistryStatus};
use zksync_os_storage_api::{ReadStateHistory, WriteState};
use zksync_os_types::L2Envelope;

use crate::config::{CommitteeEntrySource, ConsensusConfig, RegistryMode};

/// Domain-separation namespace for everything this network signs and speaks,
/// carrying the committee protocol version. Consensus messages cannot be
/// per-connection negotiated (a certificate aggregates signatures over one message
/// encoding, so the whole committee must speak one version per round) — versioning
/// the namespace makes a version mismatch fail at the handshake, loudly, instead of
/// producing garbage decodes or cross-version signature confusion.
fn namespace(protocol_version: u32) -> Vec<u8> {
    format!("zksync-os-consensus/{protocol_version}").into_bytes()
}

/// Channel ids, one per consensus traffic class. Every validator must register the
/// same set — an unrecognized channel gets its sender banned.
const VOTES: u64 = 0;
const CERTIFICATES: u64 = 1;
const CERTIFICATE_BACKFILL: u64 = 2;
const BLOCK_BROADCAST: u64 = 3;
const BLOCK_BACKFILL: u64 = 4;
const TX_GOSSIP: u64 = 5;

/// Most transactions one gossip message carries (the sender drains whatever is
/// immediately available up to this).
const MAX_TXS_PER_GOSSIP: usize = 64;

/// One committee member as configured: who they are (network identity, consensus key)
/// and where to reach them.
#[derive(Debug, Clone)]
pub struct CommitteeMember {
    pub network_key: ed25519::PublicKey,
    pub bls_key: <MinPk as Variant>::Public,
    pub address: SocketAddr,
}

/// Parses one `consensus.validators` entry: `<ed25519_hex>:<bls_hex>@<host:port>`.
pub fn parse_committee_member(entry: &str) -> anyhow::Result<CommitteeMember> {
    let (keys, address) = entry
        .split_once('@')
        .context("expected `<ed25519_hex>:<bls_hex>@<host:port>`")?;
    let (network_hex, bls_hex) = keys
        .split_once(':')
        .context("expected `<ed25519_hex>:<bls_hex>` before `@`")?;
    let network_key =
        decode_hex_key::<ed25519::PublicKey>(network_hex).context("invalid ed25519 public key")?;
    let bls_key =
        decode_hex_key::<<MinPk as Variant>::Public>(bls_hex).context("invalid BLS public key")?;
    let address: SocketAddr = address.parse().context("invalid validator address")?;
    Ok(CommitteeMember {
        network_key,
        bls_key,
        address,
    })
}

/// One observer as configured: a network identity and where to reach it. No BLS
/// key — observers never sign.
#[derive(Debug, Clone)]
pub struct ObserverPeer {
    pub network_key: ed25519::PublicKey,
    pub address: SocketAddr,
}

/// Parses one `consensus.observers` entry: `<ed25519_hex>@<host:port>`.
pub fn parse_observer_peer(entry: &str) -> anyhow::Result<ObserverPeer> {
    let (network_hex, address) = entry
        .split_once('@')
        .context("expected `<ed25519_hex>@<host:port>`")?;
    let network_key =
        decode_hex_key::<ed25519::PublicKey>(network_hex).context("invalid ed25519 public key")?;
    let address: SocketAddr = address.parse().context("invalid observer address")?;
    Ok(ObserverPeer {
        network_key,
        address,
    })
}

fn decode_hex_key<T: commonware_codec::DecodeExt<()>>(hex: &str) -> anyhow::Result<T> {
    let bytes = alloy::hex::decode(hex.trim()).context("invalid hex")?;
    T::decode(bytes.as_slice()).map_err(|err| anyhow::anyhow!("invalid key encoding: {err}"))
}

/// What startup should do about the consensus era: proceed on a match, or record
/// the (new) era. Any state that could mix two consensus histories is an error.
#[derive(Debug, PartialEq, Eq)]
pub enum EraDecision {
    /// The recorded era matches the configured anchor — normal operation.
    Proceed,
    /// Record the configured era: the first consensus start of this chain, a
    /// deliberate re-migration over cleared engine state, or an instance from
    /// before era tracking existed.
    Adopt,
}

/// The consensus-era guard, pure so the whole matrix is unit-testable. The era is
/// the consensus genesis digest (anchor height + anchored block hash): recorded at
/// the first consensus start, compared on every later one.
pub fn decide_consensus_era(
    recorded: Option<[u8; 32]>,
    configured: [u8; 32],
    engine_state_is_fresh: bool,
    wal_tip: u64,
    anchor_height: u64,
    // The operator's `consensus.acknowledge_fork`, parsed: the anchor height
    // and block hash being deliberately forked/re-migrated to.
    acknowledged_fork: Option<(u64, alloy::primitives::B256)>,
    // The hash this node's own chain has at the anchor height — what the
    // acknowledgment must name (catches a truncation that landed on the wrong
    // chain *before* this node quietly forms its own lonely era).
    local_hash_at_anchor: alloy::primitives::B256,
) -> anyhow::Result<EraDecision> {
    match (recorded, engine_state_is_fresh) {
        (Some(era), _) if era == configured => Ok(EraDecision::Proceed),
        (Some(_), false) => anyhow::bail!(
            "this chain previously ran consensus with a different anchor than \
             `consensus.genesis_height` = {anchor_height} derives. If this is a deliberate \
             re-migration after a rollback, clear the consensus engine state and restart; \
             otherwise fix the configured genesis height"
        ),
        // A different era over deliberately cleared engine state: a disaster
        // fork or a re-migration. Either way finalized history is being
        // overridden, so the operator must acknowledge exactly what they are
        // starting into — the anchor height and its hash — and the chain must
        // end exactly there.
        (Some(_), true) => {
            let (acknowledged_height, acknowledged_hash) = acknowledged_fork.context(
                "this chain previously ran consensus under a different era; starting into \
                 a new anchor abandons finalized history and requires \
                 `consensus.acknowledge_fork = \"<height>:<block hash at height>\"` \
                 naming the new anchor",
            )?;
            anyhow::ensure!(
                acknowledged_height == anchor_height,
                "`consensus.acknowledge_fork` names height {acknowledged_height} but \
                 `consensus.genesis_height` is {anchor_height} — the acknowledgment must \
                 name exactly the anchor being started into"
            );
            anyhow::ensure!(
                acknowledged_hash == local_hash_at_anchor,
                "`consensus.acknowledge_fork` names hash {acknowledged_hash} at height \
                 {anchor_height}, but this node's chain has {local_hash_at_anchor} there — \
                 the truncation on this node did not land on the agreed block; do not \
                 start it (re-check the truncation and the agreed anchor)"
            );
            anyhow::ensure!(
                wal_tip == anchor_height,
                "a consensus era must start exactly at the agreed cutover: the write-ahead \
                 log ends at {wal_tip} but `consensus.genesis_height` is {anchor_height}"
            );
            Ok(EraDecision::Adopt)
        }
        // No era at all over fresh state: the first consensus start of this
        // chain (fresh chain or first migration) — nothing finalized is being
        // overridden, no acknowledgment needed; the cutover must still be exact.
        (None, true) => {
            anyhow::ensure!(
                wal_tip == anchor_height,
                "a consensus era must start exactly at the agreed cutover: the write-ahead \
                 log ends at {wal_tip} but `consensus.genesis_height` is {anchor_height}"
            );
            Ok(EraDecision::Adopt)
        }
        // No marker over existing engine state: an instance from before era tracking
        // existed. Adopt its era (the anchor still derives it — a mismatch would have
        // broken consensus itself long before this check).
        (None, false) => Ok(EraDecision::Adopt),
    }
}

/// Parses `consensus.acknowledge_fork`: `"<height>:<block hash at height>"`.
pub fn parse_acknowledge_fork(
    value: &Option<String>,
) -> anyhow::Result<Option<(u64, alloy::primitives::B256)>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (height, hash) = value.split_once(':').context(
        "`consensus.acknowledge_fork` must be `\"<height>:<block hash at height>\"`",
    )?;
    let height: u64 = height
        .trim()
        .parse()
        .context("`consensus.acknowledge_fork` height is not a number")?;
    let hash: alloy::primitives::B256 = hash
        .trim()
        .parse()
        .context("`consensus.acknowledge_fork` hash is not a 32-byte hex hash")?;
    Ok(Some((height, hash)))
}

/// The rollback guard: single-sequencer operation over existing consensus state
/// strands that state and a later re-enable could mix histories — refuse unless the
/// operator acknowledged the rollback. Never deletes anything.
pub fn check_rollback_acknowledged(
    has_consensus_state: bool,
    acknowledged: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !has_consensus_state || acknowledged,
        "this chain has consensus state but consensus is disabled. If this rollback to \
         single-sequencer operation is deliberate, set `consensus.acknowledge_rollback: \
         true`; the consensus state is left untouched"
    );
    Ok(())
}

/// Everything the consensus thread needs, resolved and validated on the node side
/// before the thread spawns (so misconfiguration fails startup, not a background
/// thread).
pub struct ConsensusSetup {
    /// The p2p address book: every member of every schedule entry, deduplicated —
    /// future committee members must be dialable before their activation epoch.
    pub committee: Vec<CommitteeMember>,
    pub network_key: ed25519::PrivateKey,
    /// Per-epoch schemes over the committee schedule (signer where this validator
    /// is a member, verifier elsewhere).
    pub provider: SchemeProvider,
    /// The schedule itself, for consumers that need committees rather than schemes
    /// (the activity observer's custody records, the status surface).
    pub schedule: std::sync::Arc<CommitteeSchedule>,
    pub epoch_length: std::num::NonZeroU64,
    /// View timeouts, validated against the block time at config load.
    pub leader_timeout: std::time::Duration,
    pub certification_timeout: std::time::Duration,
    /// The chain height consensus is anchored at (`consensus.genesis_height`):
    /// consensus heights count from it, and decoded blocks learn it via the block
    /// codec config.
    pub era_anchor: u64,
    pub listen_address: SocketAddr,
    pub allow_private_ips: bool,
    pub max_message_size: usize,
    pub storage_directory: PathBuf,
    /// The protocol-versioned base namespace (see [`namespace`]); the signing
    /// namespace already baked it into the scheme provider, the p2p stack derives
    /// from it at startup.
    pub namespace: Vec<u8>,
    /// Use a cached finality floor even when it predates the committee's last
    /// scheduled change (`consensus.accept_stale_floor`).
    pub accept_stale_floor: bool,
    /// Retired epochs of consensus storage to keep (`consensus.epoch_retention`;
    /// `None` = keep everything).
    pub epoch_retention: Option<std::num::NonZeroU64>,
    /// Whether this node votes or only follows (`consensus.role`).
    pub role: crate::config::ConsensusRole,
    /// Admitted non-voting observers (`consensus.observers`) — tracked as the
    /// committee peer set's *secondary* tier: authorized to connect, but skipped
    /// by primary-only policies (block-broadcast caching treats only primary
    /// peers as potential proposers). Includes this node itself when it runs as
    /// an observer.
    pub observers: Vec<ObserverPeer>,
    /// The on-chain validator registry, when it participates
    /// (`consensus.registry_mode` `shadow`/`config_shadow`); `None` in `schedule`
    /// mode.
    pub registry: Option<RegistrySetup>,
}

/// Resolved registry participation (see `consensus.registry_mode`).
#[derive(Debug, Clone)]
pub struct RegistrySetup {
    pub mode: RegistryMode,
    /// L2 address of the registry contract; its storage slots are read directly
    /// out of applied chain state at each epoch's lookahead boundary.
    pub address: alloy::primitives::Address,
    /// `config_shadow` mode: the epoch recorded derivations govern from (the
    /// `source: registry` schedule entry's activation). `None` in shadow mode —
    /// derivations run and are recorded, consensus follows config.
    pub flip_epoch: Option<u64>,
    /// Proofs of possession bind to the chain id; the derivation must verify
    /// them against the same one.
    pub chain_id: u64,
}

impl ConsensusSetup {
    /// Resolves keys and the committee schedule from configuration. `chain_id`
    /// scopes the registry's proofs of possession (committee-uniform, from the
    /// genesis config).
    pub fn from_config(
        config: &ConsensusConfig,
        storage_directory: PathBuf,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        // The schedule's entries: an explicit `committees` schedule, or the
        // `validators` shorthand (one committee, activating at epoch 0). A
        // `source: registry` entry is not a committee — it is the flip point
        // where recorded registry derivations take over.
        let mut registry_flip: Option<u64> = None;
        let configured: Vec<(u64, &Vec<String>)> = if config.committees.is_empty() {
            vec![(0, &config.validators)]
        } else {
            let mut entries = Vec::new();
            for entry in &config.committees {
                match entry.source {
                    CommitteeEntrySource::Validators => {
                        entries.push((entry.activation_epoch, &entry.validators));
                    }
                    CommitteeEntrySource::Registry => {
                        anyhow::ensure!(
                            entry.validators.is_empty(),
                            "the `source: registry` schedule entry (epoch {}) lists validators — \
                             the registry supplies them; the entry must be empty",
                            entry.activation_epoch,
                        );
                        anyhow::ensure!(
                            registry_flip.is_none(),
                            "multiple `source: registry` schedule entries; configure exactly one",
                        );
                        anyhow::ensure!(
                            entry.activation_epoch >= 1,
                            "the registry cannot govern epoch 0 — the first committee \
                             bootstraps from config",
                        );
                        registry_flip = Some(entry.activation_epoch);
                    }
                }
            }
            entries
        };

        // Registry-mode cross-validation, all directions loud: an address that
        // silently does nothing and a mode that silently misses its address are
        // both misconfigurations.
        match config.registry_mode {
            RegistryMode::Schedule => {
                anyhow::ensure!(
                    config.registry_address.is_none(),
                    "`consensus.registry_address` is set but `consensus.registry_mode` is \
                     `schedule` (the registry would be ignored) — set the mode to `shadow` or \
                     drop the address",
                );
            }
            RegistryMode::Shadow | RegistryMode::ConfigShadow => {
                anyhow::ensure!(
                    config.registry_address.is_some(),
                    "`consensus.registry_mode: {}` requires `consensus.registry_address`",
                    config.registry_mode,
                );
            }
        }
        match (config.registry_mode, registry_flip) {
            (RegistryMode::ConfigShadow, None) => anyhow::bail!(
                "`consensus.registry_mode: config_shadow` requires the flip point: one \
                 `consensus.committees` entry with `source: registry`",
            ),
            (RegistryMode::ConfigShadow, Some(_)) => {}
            (_, Some(epoch)) => anyhow::bail!(
                "a `source: registry` schedule entry (epoch {epoch}) requires \
                 `consensus.registry_mode: config_shadow` (current mode: {})",
                config.registry_mode,
            ),
            (_, None) => {}
        }

        // Parse every entry, building the schedule and the address-book union. A
        // validator may appear in any number of entries, but always with the same
        // keys and address — a mismatch means two operators disagree about who a
        // validator *is*, which no amount of consensus can reconcile.
        let mut address_book: Vec<CommitteeMember> = Vec::new();
        let mut entries = Vec::new();
        for (activation_epoch, validators) in configured {
            anyhow::ensure!(
                validators.len() >= 2,
                "the committee activating at epoch {activation_epoch} has fewer than 2 \
                 validators"
            );
            let members: Vec<CommitteeMember> = validators
                .iter()
                .map(|entry| {
                    parse_committee_member(entry).with_context(|| {
                        format!("bad committee entry (epoch {activation_epoch}): {entry}")
                    })
                })
                .collect::<anyhow::Result<_>>()?;
            for member in &members {
                match address_book
                    .iter()
                    .find(|known| known.network_key == member.network_key)
                {
                    None => address_book.push(member.clone()),
                    Some(known) => anyhow::ensure!(
                        known.bls_key == member.bls_key && known.address == member.address,
                        "validator {} appears with a different BLS key or address in the \
                         committee activating at epoch {activation_epoch}",
                        member.network_key,
                    ),
                }
            }
            let committee: BiMap<ed25519::PublicKey, <MinPk as Variant>::Public> = members
                .iter()
                .map(|member| (member.network_key.clone(), member.bls_key))
                .try_collect()
                .map_err(|err| {
                    anyhow::anyhow!(
                        "duplicate member in the committee activating at epoch \
                         {activation_epoch}: {err:?}"
                    )
                })?;
            entries.push(ScheduleEntry {
                activation_epoch,
                committee,
            });
        }
        let schedule = CommitteeSchedule::new(entries)
            .context("invalid committee schedule (`consensus.committees`)")?;

        // The observers' admission list: shared verbatim by every node — validators
        // authorize observer connections from it; an observer finds itself in it.
        // Identities must be unique and disjoint from the committee's (a scheduled
        // member's key doubling as an observer would make "who is this connection"
        // ambiguous at the network layer).
        let mut observers: Vec<ObserverPeer> = Vec::new();
        for entry in &config.observers {
            let peer = parse_observer_peer(entry)
                .with_context(|| format!("bad `consensus.observers` entry: {entry}"))?;
            anyhow::ensure!(
                !observers
                    .iter()
                    .any(|known| known.network_key == peer.network_key),
                "observer {} is listed twice in `consensus.observers`",
                peer.network_key,
            );
            anyhow::ensure!(
                !address_book
                    .iter()
                    .any(|member| member.network_key == peer.network_key),
                "observer {} is also a committee member; a node is one or the other",
                peer.network_key,
            );
            observers.push(peer);
        }

        let network_key = decode_hex_key::<ed25519::PrivateKey>(
            config
                .network_key
                .as_ref()
                .context("`consensus.network_key` is required")?,
        )
        .context("invalid `consensus.network_key`")?;
        use commonware_cryptography::Signer as _;
        let our_network_key = network_key.public_key();

        // Role-dependent keys and guards, all loud on purpose: every quiet failure
        // shape here (a validator that follows but never votes, an observer the
        // committee expects votes from) costs the committee liveness margin with
        // no error anywhere.
        let signing_key = match config.role {
            crate::config::ConsensusRole::Validator => {
                let bls_key = decode_hex_key::<group::Private>(
                    config
                        .bls_key
                        .as_ref()
                        .context("`consensus.bls_key` is required for validators")?,
                )
                .context("invalid `consensus.bls_key`")?;

                // A schedule that lists this validator's network key against a
                // *different* BLS key would make every scheme build in verifier
                // mode: the node would run, follow, and simply never vote. And a
                // key in no committee at all is usually a misconfiguration, unless
                // the operator says otherwise.
                let our_bls_public =
                    commonware_cryptography::bls12381::primitives::ops::compute_public::<MinPk>(
                        &bls_key,
                    );
                let mut member_anywhere = false;
                for entry in schedule.entries() {
                    if let Some(listed) = entry.committee.get_value(&our_network_key) {
                        member_anywhere = true;
                        anyhow::ensure!(
                            listed == &our_bls_public,
                            "the committee activating at epoch {} pairs this validator's network \
                             key with a BLS public key that `consensus.bls_key` does not derive — \
                             the validator would verify but never vote; fix the schedule entry or \
                             the signing key",
                            entry.activation_epoch,
                        );
                    }
                }
                anyhow::ensure!(
                    member_anywhere || config.acknowledge_non_member,
                    "this validator's network key appears in no configured committee. If it is \
                     deliberately running as a non-voting follower (e.g. freshly scheduled out \
                     of the committee), set `consensus.acknowledge_non_member: true`; otherwise \
                     fix `consensus.validators` / `consensus.committees` — or run it as \
                     `consensus.role=observer`",
                );
                Some(bls_key)
            }
            crate::config::ConsensusRole::Observer => {
                // The inverse guards: the committee must not be counting on this
                // node's votes, and it must be in the admission list or nobody
                // will accept its connections.
                for entry in schedule.entries() {
                    anyhow::ensure!(
                        entry.committee.get_value(&our_network_key).is_none(),
                        "this node's network key is scheduled into the committee activating \
                         at epoch {}, but `consensus.role` is `observer` — the committee \
                         would wait for votes that never come; run it as a validator or fix \
                         the schedule",
                        entry.activation_epoch,
                    );
                }
                anyhow::ensure!(
                    observers
                        .iter()
                        .any(|peer| peer.network_key == our_network_key),
                    "this observer's network key is missing from `consensus.observers` — \
                     validators only accept connections from identities on that list",
                );
                None
            }
        };

        let base_namespace = namespace(config.protocol_version);
        let signing_namespace = union_unique(&base_namespace, b"_CONSENSUS");
        let committee_source = match registry_flip {
            Some(flip) => CommitteeSource::with_registry_from(schedule.clone(), flip),
            None => CommitteeSource::from_config(schedule.clone()),
        };
        let provider =
            SchemeProvider::over_source(signing_namespace, committee_source, signing_key);
        let registry = match config.registry_mode {
            RegistryMode::Schedule => None,
            mode => Some(RegistrySetup {
                mode,
                address: config.registry_address.expect("validated above"),
                flip_epoch: registry_flip,
                chain_id,
            }),
        };

        Ok(Self {
            committee: address_book,
            network_key,
            provider,
            registry,
            schedule: std::sync::Arc::new(schedule),
            epoch_length: std::num::NonZeroU64::new(config.epoch_length)
                .context("`consensus.epoch_length` must be nonzero")?,
            leader_timeout: config.leader_timeout,
            certification_timeout: config.certification_timeout,
            era_anchor: config.genesis_height,
            accept_stale_floor: config.accept_stale_floor,
            epoch_retention: std::num::NonZeroU64::new(config.epoch_retention),
            role: config.role,
            observers,
            listen_address: config
                .listen_address
                .parse()
                .context("invalid `consensus.listen_address`")?,
            allow_private_ips: config.allow_private_ips,
            max_message_size: config.max_message_size,
            storage_directory,
            namespace: base_namespace,
        })
    }
}

/// Where this validator's consensus state starts (see
/// [`zksync_os_consensus_core::StackStart`]): a cached finality floor when one is
/// usable — bounding an empty-storage start's catch-up to the blocks above it —
/// or the era genesis (full backfill). With existing consensus storage marshal
/// ignores a floor at or below what it already processed, so this selection only
/// changes the empty-storage cases: a rebuild after an incident, or a node
/// promoted into the committee with a retained chain.
fn select_stack_start(
    context: &mut commonware_runtime::tokio::Context,
    finality: &zksync_os_consensus_execution::FinalityStore,
    provider: &SchemeProvider,
    era_anchor: u64,
    chain_tip: u64,
    accept_stale_floor: bool,
) -> zksync_os_consensus_core::StackStart<commonware_cryptography::sha256::Digest> {
    use commonware_codec::Read as _;
    use commonware_cryptography::certificate::Scheme as _;
    use zksync_os_consensus_core::StackStart;

    // A floor must anchor at or below the chain tip (marshal re-delivers the floor
    // block, then everything above it; a floor above the tip would leave a delivery
    // gap the chain can never fill) and above the era anchor (the anchor itself is
    // the Genesis start). The window bounds startup work; finalizations are dense,
    // so the newest cached one is normally within a few heights of the tip.
    const WINDOW: u64 = 1024;
    const RAW_SCAN: usize = 4096;
    let low = chain_tip.saturating_sub(WINDOW).max(era_anchor + 1);
    if chain_tip < low {
        return StackStart::Genesis;
    }
    let mut heights_by_digest = std::collections::HashMap::new();
    for height in low..=chain_tip {
        if let Ok(Some(digest)) = finality.digest_at_height(height) {
            heights_by_digest.insert(digest, height);
        }
    }

    let latest_transition = finality.latest_transition_epoch();
    for (epoch, view, digest, raw) in finality.raw_finalizations_newest_first(RAW_SCAN) {
        let Some(height) = heights_by_digest.get(&digest) else {
            continue;
        };
        // Freshness policy (ratified in the EN-convergence design): a floor from
        // before the committee's last scheduled change is refused — the full
        // backfill re-derives everything instead. Entries are scanned newest
        // first, so every later candidate is staler: stop here.
        if let Some(latest) = latest_transition
            && epoch < latest
            && !accept_stale_floor
        {
            tracing::warn!(
                floor_epoch = epoch,
                latest_transition_epoch = latest,
                "cached finality floor predates the last committee change; \
                 falling back to a full backfill (set `consensus.accept_stale_floor` \
                 to use it anyway)"
            );
            return StackStart::Genesis;
        }
        // Cache semantics: an entry that no longer decodes or verifies is skipped,
        // not fatal — entries fail independently. A consensus-library upgrade
        // invalidates all of them (the scan falls through to Genesis); a corrected
        // committee schedule invalidates only the entries a misconfigured node
        // "verified" under its stale scheme (a stalled validator's cache really
        // does hold stale-width certificates for the epoch it stalled in), and an
        // older, genuinely-valid floor behind them is still worth finding.
        let scheme = provider.scheme_for(zksync_os_consensus_core::types::Epoch::new(epoch));
        let Ok(finalization) =
            zksync_os_consensus_core::types::Finalization::<
                zksync_os_consensus_core::types::Scheme,
                commonware_cryptography::sha256::Digest,
            >::read_cfg(&mut raw.as_slice(), &scheme.certificate_codec_config())
        else {
            tracing::warn!(
                epoch,
                view,
                "skipping a cached finality floor that no longer decodes (library \
                 upgrade, or a certificate recorded under a corrected-away schedule)"
            );
            continue;
        };
        if finalization.proposal.payload.as_ref() != digest
            || !finalization.verify(
                context,
                scheme.as_ref(),
                &zksync_os_consensus_core::types::Sequential,
            )
        {
            tracing::warn!(
                epoch,
                view,
                "skipping a cached finality floor that does not verify under the \
                 configured schedule"
            );
            continue;
        }
        tracing::info!(
            height,
            epoch,
            view,
            "consensus will start from a cached finality floor if its storage is empty"
        );
        return StackStart::Floor(Box::new(finalization));
    }
    StackStart::Genesis
}

/// Spawns the consensus world. Returns the thread handle and a receiver that fires when
/// consensus dies — the node must treat that as fatal.
///
/// `shutdown` asks consensus to stop gracefully (releasing its p2p listener and
/// journals); the *sender being dropped* counts as that request too, so holding the
/// sender inside a node-runtime task makes node shutdown stop consensus automatically.
pub fn spawn<S, P>(
    setup: ConsensusSetup,
    env: NodeExecutionEnv<S>,
    l2_pool: P,
    observability: ConsensusObservability,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> (
    std::thread::JoinHandle<anyhow::Result<()>>,
    tokio::sync::oneshot::Receiver<()>,
)
where
    S: ReadStateHistory + WriteState + Clone + Send + Sync + 'static,
    P: L2Subpool + Clone,
{
    let (dead_sender, dead_receiver) = tokio::sync::oneshot::channel();
    let handle = std::thread::Builder::new()
        .name("consensus".to_string())
        .spawn(move || {
            let result = run(setup, env, l2_pool, observability, shutdown);
            // The reason must hit the logs *here*: the JoinHandle's return value
            // is nowhere read, and the watchdog that reacts to the death signal
            // only knows "consensus died" — which failure arm fired (networking,
            // rotation, marshal, broadcast, shutdown timeout) is exactly what an
            // on-call engineer needs to pick a remedy.
            match &result {
                Ok(()) => tracing::info!("consensus stack exited cleanly"),
                Err(reason) => tracing::error!(?reason, "consensus stack died"),
            }
            // Fire unconditionally: the node must learn about consensus death whether
            // it was an error or a clean shutdown (the watchdog is already gone then).
            let _ = dead_sender.send(());
            result
        })
        .expect("failed to spawn consensus thread");
    (handle, dead_receiver)
}

fn run<S, P>(
    setup: ConsensusSetup,
    env: NodeExecutionEnv<S>,
    l2_pool: P,
    observability: ConsensusObservability,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()>
where
    S: ReadStateHistory + WriteState + Clone + Send + Sync + 'static,
    P: L2Subpool + Clone,
{
    // One consensus instance per storage directory, ever. A restarting node must not
    // open the journals while the previous instance is still flushing them, and two
    // live instances sharing them would corrupt the vote journal (double-sign risk).
    // The advisory lock is per file handle: held for this thread's lifetime, released
    // by the OS even on a crash.
    std::fs::create_dir_all(&setup.storage_directory)
        .context("failed to create the consensus storage directory")?;
    let storage_lock = std::fs::File::create(instance_lock_path(&setup.storage_directory))
        .context("failed to open the consensus storage lock")?;
    if fs2::FileExt::try_lock_exclusive(&storage_lock).is_err() {
        tracing::info!(
            "waiting for a previous consensus instance to release its storage before starting"
        );
        fs2::FileExt::lock_exclusive(&storage_lock)
            .context("failed to lock the consensus storage")?;
    }

    let runtime_config = commonware_runtime::tokio::Config::default()
        .with_tcp_nodelay(Some(true))
        .with_worker_threads(3)
        .with_storage_directory(setup.storage_directory.clone())
        .with_catch_panics(false);
    let runner = commonware_runtime::tokio::Runner::new(runtime_config);
    // Held across the runtime's lifetime and dropped on this plain thread afterwards:
    // the environment (via the block builder's mempool) embeds the node's task-runtime
    // handle, and the last handle must not be dropped inside an async worker (dropping
    // a runtime in async context panics). This clone outlives every in-task clone.
    let env_anchor = env.clone();
    // The consensus runtime's executor must take its last breath on this plain
    // thread: every `Context` clone holds a reference to it, and whichever thread
    // drops the last one tears the runtime down — doing that inside an async worker
    // panics ("cannot drop a runtime where blocking is not allowed"). The channel
    // smuggles one context out to this thread, which then outlives every in-task and
    // node-side holder and performs the teardown safely.
    let (context_anchor_sender, context_anchor) = std::sync::mpsc::channel();
    let metrics_encoder = observability.metrics_encoder;
    let metrics_encoder_in_runtime = metrics_encoder.clone();
    let result = runner.start(|context| async move {
        let _ = context_anchor_sender.send(context.child("teardown_anchor"));
        // From here on the consensus runtime's own registry (engine, marshal, p2p) is
        // live; hand the node a way to scrape it.
        let _ = metrics_encoder_in_runtime.send(Some(std::sync::Arc::new({
            let encoder_context = context.child("metrics_encoder");
            move || encoder_context.encode()
        })));

        let quota = Quota::per_second(NonZeroU32::new(128).expect("nonzero"));
        // Block traffic is bulkier and rarer than votes; keep its rate low so backfill
        // cannot starve the vote channels.
        let block_quota = Quota::per_second(NonZeroU32::new(8).expect("nonzero"));
        const BACKLOG: usize = 16_384;

        // TODO(consensus): timing/rate/backlog constants here and in `StackConfig` are
        // fixed at values suitable for small committees on good links; expose the ones
        // staging shows a need to tune (leader timeout, quotas, message size already is).
        let p2p_config = lookup::Config {
            namespace: union_unique(&setup.namespace, b"_P2P"),
            crypto: setup.network_key.clone(),
            listen: setup.listen_address,
            max_message_size: setup.max_message_size as u32,
            mailbox_size: commonware_utils::NZUsize!(16_384),
            send_batch_size: commonware_utils::NZUsize!(8),
            bypass_ip_check: false,
            allow_private_ips: setup.allow_private_ips,
            allow_dns: true,
            tracked_peer_sets: commonware_utils::NZUsize!(3),
            synchrony_bound: std::time::Duration::from_secs(5),
            max_handshake_age: std::time::Duration::from_secs(10),
            handshake_timeout: std::time::Duration::from_secs(5),
            max_concurrent_handshakes: NonZeroU32::new(512).expect("nonzero"),
            block_duration: std::time::Duration::from_secs(4 * 60 * 60),
            dial_frequency: std::time::Duration::from_secs(1),
            ping_frequency: std::time::Duration::from_secs(50),
            // Validators start together (deploys, tests), so first dials routinely race
            // each other or a not-yet-bound listener. A long cooldown after such a
            // failure delays committee formation by that much — keep it short; the dial
            // frequency already bounds redial traffic.
            peer_connection_cooldown: std::time::Duration::from_secs(5),
            // Committee members may share an IP (co-located validators, every node in
            // an in-process test). The limit only shields against handshake floods, so
            // it just needs to comfortably exceed committee size.
            allowed_handshake_rate_per_ip: Quota::per_second(NonZeroU32::new(64).expect("nonzero")),
            allowed_handshake_rate_per_subnet: Quota::per_second(
                NonZeroU32::new(64).expect("nonzero"),
            ),
        };
        let (mut network, mut oracle) = lookup::Network::new(context.child("p2p"), p2p_config);

        // The static committee is peer set 0; validator-set changes later mean
        // tracking new sets under new indices.
        //
        // TODO(consensus): peer tracking ignores the registry. This one call is
        // the network's whole address book, built once at startup from the
        // config schedule; registry derivations later change *which keys* form
        // a committee, but never which addresses are dialable. Two concrete
        // consequences while that holds: (1) a committee member only the
        // registry names is unreachable — in `config_shadow` mode the config
        // mirror is what keeps every member dialable, so a mirror that lags a
        // registry rotation costs the new member's connectivity until the
        // mirror deploys (the drift alarm flags the lag); (2) the registry's
        // self-service endpoint updates (`setEndpoints`) have no effect on a
        // running committee. Resolve when adding the future registry-only
        // `contract` mode (no config mirror to lean on): on each derivation,
        // register the derived committee's registry endpoints as a new peer-set
        // generation here (`oracle.track(next_index, ...)` — tracked sets are
        // generations, which is upstream's committee-transition mechanism), and
        // revisit the `member_of_any` startup guard for registry-only members.
        let peers: Map<ed25519::PublicKey, Address> = setup
            .committee
            .iter()
            .map(|member| {
                (
                    member.network_key.clone(),
                    Address::Asymmetric {
                        ingress: Ingress::Socket(member.address),
                        egress: SocketAddr::from((member.address.ip(), 0)),
                    },
                )
            })
            .try_collect()
            .expect("duplicate validator network identity");
        // Observers ride in the same tracked set as the committee, as its
        // *secondary* tier: tracked identities complete handshakes (this is the
        // observers' admission perimeter — see `consensus.observers`), but
        // primary-only policies skip them — notably the block-broadcast cache,
        // which only accepts blocks from primary peers, i.e. potential proposers.
        // Deliberately not a second peer-set index: set indexes are generations
        // (the committee-transition overlap mechanism), and components treat the
        // latest generation as *the* network — a separate observers set would
        // supersede the committee and stall block dissemination.
        let observer_peers: Map<ed25519::PublicKey, Address> = setup
            .observers
            .iter()
            .map(|peer| {
                (
                    peer.network_key.clone(),
                    Address::Asymmetric {
                        ingress: Ingress::Socket(peer.address),
                        egress: SocketAddr::from((peer.address.ip(), 0)),
                    },
                )
            })
            .try_collect()
            .expect("duplicate observer identity");
        let peers = commonware_p2p::AddressableTrackedPeers::new(peers, observer_peers);
        let _ = oracle.track(0, peers);

        // Channels must all be registered before the network starts.
        let channels = Channels {
            votes: network.register(VOTES, quota, BACKLOG),
            certificates: network.register(CERTIFICATES, quota, BACKLOG),
            certificate_backfill: network.register(CERTIFICATE_BACKFILL, quota, BACKLOG),
            block_broadcast: network.register(BLOCK_BROADCAST, block_quota, BACKLOG),
            block_backfill: network.register(BLOCK_BACKFILL, block_quota, BACKLOG),
        };
        let (tx_gossip_sender, tx_gossip_receiver) = network.register(TX_GOSSIP, quota, BACKLOG);
        let network_handle = network.start();

        start_tx_gossip(
            &context,
            l2_pool,
            tx_gossip_sender,
            tx_gossip_receiver,
            setup.max_message_size,
            setup.role,
        );

        use commonware_cryptography::Signer as _;
        let identity = setup.network_key.public_key();

        // The registry derivation trail replays into the committee source before
        // anything resolves committees through the provider: floor selection
        // below verifies cached finalizations, and the consensus stack verifies
        // certificates from its first moment — under a registry flip, both are
        // only correct once the recorded derivations are back in memory.
        let registry_resume = setup.registry.clone().map(|registry| {
            use zksync_os_consensus_core::{DerivationLedger as _, replay_ledger};
            use zksync_os_consensus_execution::registry_source::RegistryLedger;
            let ledger = RegistryLedger(observability.finality.clone());
            // A trail that no longer decodes is corrupt storage — refuse loudly
            // rather than silently re-deriving what may no longer be derivable.
            let records = ledger
                .load()
                .expect("the registry derivation trail does not decode");
            let newest_recorded = replay_ledger(setup.provider.source(), &records);
            (registry, ledger, newest_recorded)
        });

        let start = {
            let mut committed_probe = env.clone();
            let committed =
                zksync_os_consensus_core::ExecutionEnv::committed_height(&mut committed_probe)
                    .await
                    .map(|height| height.get())
                    .unwrap_or(0);
            // `committed_height` is era-relative; the finality store's height index
            // is chain-absolute.
            let chain_tip = setup.era_anchor + committed;
            let mut floor_context = context.child("floor_select");
            select_stack_start(
                &mut floor_context,
                &observability.finality,
                &setup.provider,
                setup.era_anchor,
                chain_tip,
                setup.accept_stale_floor,
            )
        };
        let stack = start_validator(
            context.child("validator"),
            {
                let mut stack_config =
                    StackConfig::new("consensus").with_epoch_length(setup.epoch_length);
                stack_config.epoch_retention = setup.epoch_retention;
                stack_config.leader_timeout = setup.leader_timeout;
                stack_config.certification_timeout = setup.certification_timeout;
                stack_config
            },
            identity,
            setup.provider.clone(),
            env.clone(),
            oracle.clone(),
            oracle,
            channels,
            // Decoded blocks learn the era anchor through the codec config: consensus
            // heights are era-relative (the anchor is consensus height zero).
            setup.era_anchor,
            ActivityObserver {
                finalized: std::sync::Arc::new(observability.finalized),
                finality: observability.finality.clone(),
                committees: setup.provider.source().clone(),
            },
            start,
        )
        .await;

        // The registry derivation: reads the validator registry out of applied
        // chain state at every epoch's lookahead boundary, records the outcome
        // durably, and feeds the committee source (which decides whether the
        // recordings govern — `config_shadow` mode — or only shadow the config).
        if let Some((registry, ledger, newest_recorded)) = registry_resume {
            use zksync_os_consensus_core::{first_live_target, run_registry_derivation};
            use zksync_os_consensus_execution::registry_source::StateDerivationSource;
            let applied_watch = env.applied_subscription();
            let applied_now = (*applied_watch.borrow()).unwrap_or(setup.era_anchor);
            let initial_target = match registry.flip_epoch {
                // `config_shadow` mode: the trail must stay dense from the flip on —
                // resume exactly after it (state unavailability at an old
                // boundary alarms rather than skips).
                Some(flip) => newest_recorded.map_or(flip, |newest| (newest + 1).max(flip)),
                // Shadow mode: coverage, not custody — boundaries that passed
                // while this node was down are skipped (their state may be
                // pruned; other nodes' trails cover them).
                None => {
                    let live = first_live_target(setup.era_anchor, setup.epoch_length, applied_now);
                    let resume = newest_recorded.map_or(live, |newest| newest + 1);
                    if resume < live {
                        tracing::info!(
                            from_epoch = resume,
                            to_epoch = live,
                            "shadow registry derivation skips epochs whose lookahead \
                             boundaries passed while this node was down"
                        );
                    }
                    resume.max(live)
                }
            };
            let source = StateDerivationSource::new(
                env.state_backend(),
                registry.address,
                registry.chain_id,
            );
            let committees = setup.provider.source().clone();
            let status = observability.registry.clone();
            let mode = registry.mode;
            let era_anchor = setup.era_anchor;
            let epoch_length = setup.epoch_length;
            // Dialability is config's job (the address book above is built from
            // it), so a derived committee reaching beyond the config mirror
            // deserves its own warning: those members hold votes the network
            // cannot deliver until the mirror deploys. Node-local observation
            // only — the derivation outcome itself must never depend on this
            // node's config timing.
            let flip_epoch = registry.flip_epoch;
            let address_book: std::collections::BTreeSet<ed25519::PublicKey> = setup
                .committee
                .iter()
                .map(|member| member.network_key.clone())
                .collect();
            context.child("registry_derivation").spawn(move |ctx| {
                run_registry_derivation(
                    ctx,
                    era_anchor,
                    epoch_length,
                    initial_target,
                    move || *applied_watch.borrow(),
                    source,
                    ledger,
                    committees,
                    move |observation| {
                        if flip_epoch.is_some_and(|flip| observation.epoch >= flip) {
                            let undialable: Vec<String> = observation
                                .committee
                                .iter_pairs()
                                .map(|(network_key, _)| network_key)
                                .filter(|network_key| !address_book.contains(network_key))
                                .map(|network_key| {
                                    use commonware_codec::Encode as _;
                                    alloy::hex::encode(network_key.encode())
                                })
                                .collect();
                            if !undialable.is_empty() {
                                tracing::warn!(
                                    epoch = observation.epoch,
                                    members = ?undialable,
                                    "the registry-derived committee has members outside \
                                     the config address book; they are not dialable until \
                                     a config mirror entry listing them deploys"
                                );
                            }
                        }
                        let _ = status.send_replace(Some(registry_status(mode, &observation)));
                    },
                )
            });
        }

        // Any component exiting is fatal: these tasks run for the life of the node.
        // The shutdown arm (fired explicitly or by the node runtime dropping the
        // sender) is the one non-fatal exit.
        let outcome = tokio::select! {
            _ = shutdown => {
                tracing::info!("node is shutting down; stopping consensus");
                Ok(())
            }
            _ = network_handle => Err(anyhow::anyhow!("consensus networking exited unexpectedly")),
            _ = stack.epoch_manager => {
                Err(anyhow::anyhow!("consensus epoch rotation exited unexpectedly"))
            }
            _ = stack.marshal => Err(anyhow::anyhow!("consensus marshal exited unexpectedly")),
            _ = stack.broadcast => Err(anyhow::anyhow!("consensus broadcast exited unexpectedly")),
        };
        // On every exit path — graceful or fatal — signal all consensus tasks to
        // stop and wait until they actually have. This must not give up on a
        // deadline: a task still winding down may be mid journal write, and
        // releasing the storage lock (below) while it lives hands the next
        // instance a vote journal that mutates under it. Better a loud, visibly
        // stuck shutdown than that. Anything still running here is wedged on
        // I/O contention and does finish; the warning gives it a name if it
        // ever truly hangs.
        while let Err(err) = context
            .child("stop")
            .stop(0, Some(std::time::Duration::from_secs(30)))
            .await
        {
            tracing::warn!(
                ?err,
                "consensus tasks are still winding down; holding the storage lock until they finish"
            );
        }
        outcome
    });
    drop(env_anchor);
    // Withdraw the metrics encoder: it captures a runtime context, and leaving it in
    // the node's status watch would keep this dead runtime alive across a consensus
    // restart — and drop it inside an async context eventually. The replaced value
    // drops right here, on this plain thread.
    let _ = metrics_encoder.send(None);
    // The teardown itself: the last context reference goes, on this thread.
    drop(context_anchor);
    // Only now — with every consensus task gone and the runtime torn down — may the
    // next instance open this storage.
    drop(storage_lock);
    result
}

/// Starts both halves of committee transaction gossip on the consensus runtime.
///
/// Outbound: every transaction newly inserted into this node's L2 pool — whether it
/// arrived over RPC or from a peer — is offered to the whole committee once. The pool
/// does not announce transactions it already knows, which is what keeps the flood
/// from echoing forever while still letting any holder heal a lost delivery.
///
/// Inbound: gossiped transactions go through the same decoding and pool validation as
/// local RPC submissions; duplicates and invalid ones die in the pool. Peers are
/// authenticated committee members, so gossip adds no new spam surface beyond what
/// each validator's own RPC already accepts.
fn start_tx_gossip<C, P, TxSender, TxReceiver>(
    context: &C,
    pool: P,
    sender: TxSender,
    mut receiver: TxReceiver,
    max_message_size: usize,
    role: crate::config::ConsensusRole,
) where
    C: commonware_runtime::Spawner + commonware_runtime::Metrics,
    P: L2Subpool + Clone,
    TxSender: Sender<PublicKey = ed25519::PublicKey>,
    TxReceiver: Receiver<PublicKey = ed25519::PublicKey>,
{
    // Leave generous headroom under the network's message cap; a batch is cut early
    // when it grows past this.
    let byte_budget = max_message_size / 2;

    // Observers receive gossip (the channel is registered either way — an
    // unrecognized channel would get the *sender* banned) but do not broadcast:
    // their transactions travel to validators over RPC forwarding instead. Gossip
    // injection from observers is the ratified later step, not this one.
    if role.is_validator() {
        let gossip_pool = pool.clone();
        start_tx_gossip_out(context, gossip_pool, sender, byte_budget);
    }

    context
        .child("tx_gossip_in")
        .spawn(move |task_context| async move {
            // `recv` errors once the network tears down, but watch the stop signal
            // too so this task never outlives the runtime with its pool handle.
            let mut stopped = task_context.stopped();
            loop {
                let (peer, message) = tokio::select! {
                    _ = &mut stopped => return,
                    received = receiver.recv() => match received {
                        Ok(received) => received,
                        Err(_) => return,
                    },
                };
                CONSENSUS_METRICS.tx_gossip[&"received"].inc();
                let Ok(batch) = <Vec<alloy::primitives::Bytes> as alloy_rlp::Decodable>::decode(
                    &mut message.as_ref(),
                ) else {
                    tracing::debug!(?peer, "undecodable transaction gossip; ignoring");
                    CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                    continue;
                };
                for tx_bytes in batch {
                    let Ok(envelope) = L2Envelope::decode_2718(&mut tx_bytes.as_ref()) else {
                        tracing::debug!(?peer, "undecodable gossiped transaction; ignoring");
                        CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                        continue;
                    };
                    let Ok(transaction) = envelope.try_into_recovered() else {
                        tracing::debug!(
                            ?peer,
                            "gossiped transaction with a bad signature; ignoring"
                        );
                        CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                        continue;
                    };
                    match pool.add_gossiped_transaction(transaction).await {
                        Ok(_) => {
                            CONSENSUS_METRICS.tx_gossip[&"admitted"].inc();
                        }
                        Err(error) => {
                            // Routine: the pool already knows most re-gossiped
                            // transactions.
                            tracing::debug!(%error, "gossiped transaction not admitted");
                            CONSENSUS_METRICS.tx_gossip[&"ignored"].inc();
                        }
                    }
                }
            }
        });
}

/// The outbound half of transaction gossip: drains the pool's new-transaction
/// stream into batched broadcasts. Validators only (see [`start_tx_gossip`]).
fn start_tx_gossip_out<C, P, TxSender>(
    context: &C,
    pool: P,
    mut sender: TxSender,
    byte_budget: usize,
) where
    C: commonware_runtime::Spawner + commonware_runtime::Metrics,
    P: L2Subpool + Clone,
    TxSender: Sender<PublicKey = ed25519::PublicKey>,
{
    context
        .child("tx_gossip_out")
        .spawn(move |task_context| async move {
            // The pool's listener never closes on consensus shutdown (the pool lives
            // node-side), so this task must watch the stop signal itself — a parked
            // task would hold pool handles (and the databases under them) past the
            // runtime's shutdown deadline.
            let mut stopped = task_context.stopped();
            let mut new_txs = pool.new_transactions_listener();
            loop {
                let event = tokio::select! {
                    _ = &mut stopped => return,
                    event = new_txs.recv() => match event {
                        Some(event) => event,
                        None => return,
                    },
                };
                // Greedily drain whatever else is already queued into one message.
                let mut batch = vec![encode_gossiped_tx(&event)];
                let mut batch_bytes = batch[0].len();
                while batch.len() < MAX_TXS_PER_GOSSIP && batch_bytes < byte_budget {
                    match new_txs.try_recv() {
                        Ok(event) => {
                            let encoded = encode_gossiped_tx(&event);
                            batch_bytes += encoded.len();
                            batch.push(encoded);
                        }
                        Err(_) => break,
                    }
                }
                let message = alloy_rlp::encode(&batch);
                // `send` is synchronous and returns the delivery list; gossip is
                // best-effort, so an empty delivery is not an error (network teardown
                // is caught by the stop signal above).
                let _ = sender.send(commonware_p2p::Recipients::All, message, false);
            }
        });
}

/// Path of the advisory lock that serializes consensus instances on one storage
/// directory. Also useful to *observe*: whoever can take this lock knows no consensus
/// instance (with everything it holds) is alive on this storage.
pub fn instance_lock_path(storage_directory: &std::path::Path) -> PathBuf {
    storage_directory.join(".instance-lock")
}

/// The canonical wire form of one gossiped transaction: its EIP-2718 encoding — the
/// exact bytes a user would submit over RPC.
fn encode_gossiped_tx(
    event: &zksync_os_mempool::NewTransactionEvent<zksync_os_mempool::L2PooledTransaction>,
) -> alloy::primitives::Bytes {
    let (envelope, _signer) = event.transaction.to_consensus().into_parts();
    envelope.encoded_2718().into()
}

/// Handles through which the consensus world reports its progress to the node's
/// status/metrics surfaces. All senders; the receivers live in the status server.
pub struct ConsensusObservability {
    /// The latest finalized round this validator observed.
    pub finalized: tokio::sync::watch::Sender<Option<FinalizedObservation>>,
    /// Installed once the consensus runtime is up: encodes its prometheus registry
    /// (engine, marshal, p2p actors) on demand.
    pub metrics_encoder: tokio::sync::watch::Sender<Option<ConsensusMetricsEncoder>>,
    /// The node's sovereign finality store: every observed finalization certificate
    /// is converted to the node's own format and persisted here.
    pub finality: std::sync::Arc<zksync_os_consensus_execution::FinalityStore>,
    /// The latest registry derivation (shadow/config_shadow modes; stays `None` in
    /// `schedule` mode and on nodes without a registry).
    pub registry: tokio::sync::watch::Sender<Option<RegistryStatus>>,
}

/// The status-surface form of one derivation observation. The committee hash is
/// the cross-node comparison handle (like the chain fingerprint: the first 8
/// bytes of a canonical sha256, hex) — two nodes disagreeing on it for the same
/// epoch is registry drift even when both individually report `matches_config`.
fn registry_status(
    mode: RegistryMode,
    observation: &zksync_os_consensus_core::RegistryObservation,
) -> RegistryStatus {
    use commonware_codec::Encode as _;
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for (network_key, bls_key) in observation.committee.iter_pairs() {
        hasher.update(network_key.encode());
        hasher.update(bls_key.encode());
    }
    RegistryStatus {
        mode: mode.as_str().to_string(),
        last_epoch: observation.epoch,
        last_lookahead_height: observation.lookahead_height,
        outcome: match observation.outcome {
            zksync_os_consensus_core::RecordedOutcome::Derived => "derived",
            zksync_os_consensus_core::RecordedOutcome::CarriedNoEntry => "carried_no_entry",
            zksync_os_consensus_core::RecordedOutcome::CarriedRefused => "carried_refused",
        }
        .to_string(),
        matches_config: observation.matches_config,
        refusal: observation.refusal.clone(),
        committee_hash: alloy::hex::encode(&Sha256::finalize(hasher)[..8]),
        committee_size: observation.committee.len(),
    }
}

/// Feeds consensus activity into metrics and the status tip. Fault evidence — proof a
/// committee member signed contradicting votes — is the loudest signal a validator
/// can produce: it must stay absent on a healthy committee.
#[derive(Clone)]
struct ActivityObserver {
    finalized: std::sync::Arc<tokio::sync::watch::Sender<Option<FinalizedObservation>>>,
    finality: std::sync::Arc<zksync_os_consensus_execution::FinalityStore>,
    /// Certificates carry per-epoch signer bitmaps, and the custody records name
    /// per-epoch committees — both resolve through the committee source (which,
    /// under a registry flip, is more than the config schedule).
    committees: CommitteeSource,
}

impl zksync_os_consensus_core::types::Reporter for ActivityObserver {
    type Activity = ConsensusActivity;

    fn report(&mut self, activity: Self::Activity) -> commonware_actor::Feedback {
        // Every vote or certificate names its round; the highest one ever seen is
        // persisted as the recovery floor for journal-loss restarts (see
        // `FinalityStore::note_observed_round`). Fault-evidence kinds are skipped —
        // their rounds ride inside the evidence pairs, and the votes they contain
        // were already observed individually.
        let observed_round = match &activity {
            Activity::Notarize(vote) => Some(vote.round()),
            Activity::Notarization(certificate) => Some(certificate.round()),
            Activity::Certification(certificate) => Some(certificate.round()),
            Activity::Nullify(vote) => Some(vote.round()),
            Activity::Nullification(certificate) => Some(certificate.round()),
            Activity::Finalize(vote) => Some(vote.round()),
            Activity::Finalization(finalization) => Some(finalization.round()),
            Activity::ConflictingNotarize(_)
            | Activity::ConflictingFinalize(_)
            | Activity::NullifyFinalize(_) => None,
        };
        if let Some(round) = observed_round
            && let Err(err) = self
                .finality
                .note_observed_round(round.epoch().get(), round.view().get())
        {
            tracing::error!(?err, "failed to persist the observed-round floor");
        }

        let kind = match &activity {
            Activity::Notarize(_) => "notarize",
            Activity::Notarization(_) => "notarization",
            Activity::Certification(_) => "certification",
            Activity::Nullify(_) => "nullify",
            Activity::Nullification(_) => "nullification",
            Activity::Finalize(_) => "finalize",
            Activity::Finalization(finalization) => {
                let round = finalization.round();
                let (epoch_committee, _) = self.committees.resolve(round.epoch());
                let committee_size = epoch_committee.len() as u32;
                // Finality is monotone, so the published observation must be too.
                // Finalizations do not arrive in round order here: the tip scout
                // re-hears certificates for already-retired epochs (a lagging peer
                // catching up re-broadcasts them, and with no engine registered for
                // that epoch they fall through to the scout), and marshal replays
                // finalizations during backfill. Without the clamp, a stale
                // re-heard finalization would move `/status.finalized` backwards on
                // a perfectly healthy validator. The durable observed-round floor
                // clamps internally already (`FinalityStore::note_observed_round`).
                let _ = self.finalized.send_if_modified(|current| {
                    let observed = (round.epoch().get(), round.view().get());
                    let advances = current
                        .as_ref()
                        .is_none_or(|seen| observed > (seen.epoch, seen.view));
                    if advances {
                        *current = Some(FinalizedObservation {
                            epoch: round.epoch().get(),
                            view: round.view().get(),
                            committee_size,
                            observed_unix: unix_now(),
                        });
                    }
                    advances
                });
                let block_digest: [u8; 32] = finalization
                    .proposal
                    .payload
                    .as_ref()
                    .try_into()
                    .expect("consensus digests are 32 bytes");
                // The sovereign copy: convert the certificate out of the consensus
                // library's types the moment it exists, so the durable record never
                // depends on the library's encoding staying stable.
                let signers: Vec<u32> = finalization
                    .certificate
                    .signers
                    .iter()
                    .map(|participant| participant.get())
                    .collect();
                let mut signature = Vec::new();
                commonware_codec::Write::write(&finalization.certificate.signature, &mut signature);
                let certificate = zksync_os_wire::FinalityCertificate {
                    scheme: zksync_os_wire::SignatureScheme::Bls12381Multisig,
                    epoch: round.epoch().get(),
                    view: round.view().get(),
                    block_digest,
                    committee_size,
                    signers: zksync_os_wire::FinalityCertificate::bitmap_from_positions(
                        committee_size,
                        &signers,
                    ),
                    signature,
                };
                if let Err(err) = self.finality.put_certificate(&certificate) {
                    tracing::error!(?err, "failed to persist a finality certificate");
                }
                // The floor cache: the same finalization in the consensus library's
                // own encoding, so a restart with empty consensus storage can hand
                // marshal a floor (the sovereign certificate cannot reconstruct
                // one). Cache semantics — see `FinalityCF::FloorCache`.
                {
                    use commonware_codec::Encode as _;
                    let raw = finalization.encode();
                    if let Err(err) = self.finality.put_raw_finalization(
                        round.epoch().get(),
                        round.view().get(),
                        block_digest,
                        raw.as_ref(),
                    ) {
                        tracing::error!(?err, "failed to cache a raw finalization");
                    }
                }
                // The custody trail: the first observed finalization of each epoch
                // records which committee holds it (first-observed wins; replays
                // change nothing).
                let transition = zksync_os_wire::EpochTransition {
                    epoch: round.epoch().get(),
                    scheme: zksync_os_wire::SignatureScheme::Bls12381Multisig,
                    committee: epoch_committee
                        .iter_pairs()
                        .map(|(network_key, bls_key)| {
                            use commonware_codec::Encode as _;
                            zksync_os_wire::CommitteeMemberKeys {
                                network_key: network_key
                                    .encode()
                                    .as_ref()
                                    .try_into()
                                    .expect("ed25519 public keys encode to 32 bytes"),
                                bls_key: bls_key
                                    .encode()
                                    .as_ref()
                                    .try_into()
                                    .expect("BLS12-381 MinPk public keys encode to 48 bytes"),
                            }
                        })
                        .collect(),
                    first_finalized_digest: block_digest,
                    first_finalized_view: round.view().get(),
                };
                match self.finality.record_epoch_transition(&transition) {
                    Ok(true) => {
                        tracing::info!(
                            epoch = transition.epoch,
                            committee_size,
                            "recorded committee custody entry for epoch"
                        );
                        // Keep the floor cache to the current and previous epoch —
                        // anything older fails the freshness policy anyway.
                        if let Some(keep_from) = transition.epoch.checked_sub(1)
                            && let Err(err) = self.finality.prune_raw_finalizations_below(keep_from)
                        {
                            tracing::warn!(?err, "failed to prune the floor cache");
                        }
                    }
                    Ok(false) => {}
                    Err(err) => {
                        tracing::error!(?err, "failed to persist an epoch transition record")
                    }
                }
                "finalization"
            }
            Activity::ConflictingNotarize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: conflicting notarize votes"
                );
                "conflicting_notarize"
            }
            Activity::ConflictingFinalize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: conflicting finalize votes"
                );
                "conflicting_finalize"
            }
            Activity::NullifyFinalize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: nullify and finalize in one view"
                );
                "nullify_finalize"
            }
        };
        CONSENSUS_METRICS.activity[&kind].inc();
        commonware_actor::Feedback::Ok
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERA_A: [u8; 32] = [0xA; 32];
    const ERA_B: [u8; 32] = [0xB; 32];

    #[test]
    fn era_guard_covers_the_whole_matrix() {
        let hash_at_anchor = alloy::primitives::B256::repeat_byte(0x1D);
        let wrong_hash = alloy::primitives::B256::repeat_byte(0x2E);
        let no_fork = || (None, hash_at_anchor);

        // Normal operation: the recorded era matches, at any WAL tip.
        assert_eq!(
            decide_consensus_era(Some(ERA_A), ERA_A, false, 500, 0, no_fork().0, no_fork().1)
                .unwrap(),
            EraDecision::Proceed
        );

        // First consensus start: fresh everything, exactly at the cutover — no
        // acknowledgment needed, nothing finalized is being overridden.
        assert_eq!(
            decide_consensus_era(None, ERA_A, true, 20, 20, None, hash_at_anchor).unwrap(),
            EraDecision::Adopt
        );
        // ... but never off the cutover (the sequencer ran past the agreed anchor,
        // or the node is missing history).
        assert!(decide_consensus_era(None, ERA_A, true, 25, 20, None, hash_at_anchor).is_err());
        assert!(decide_consensus_era(None, ERA_A, true, 15, 20, None, hash_at_anchor).is_err());

        // A fork / re-migration: a different era over cleared engine state.
        // Overriding finalized history demands the acknowledgment, naming
        // exactly this anchor and this node's hash there.
        let err = decide_consensus_era(Some(ERA_A), ERA_B, true, 40, 40, None, hash_at_anchor)
            .unwrap_err();
        assert!(err.to_string().contains("acknowledge_fork"), "got: {err}");
        assert!(
            decide_consensus_era(
                Some(ERA_A),
                ERA_B,
                true,
                40,
                40,
                Some((39, hash_at_anchor)),
                hash_at_anchor,
            )
            .is_err(),
            "an acknowledgment naming the wrong height must refuse"
        );
        assert!(
            decide_consensus_era(
                Some(ERA_A),
                ERA_B,
                true,
                40,
                40,
                Some((40, wrong_hash)),
                hash_at_anchor,
            )
            .is_err(),
            "an acknowledgment naming a hash this chain does not have must refuse \
             (the truncation landed wrong)"
        );
        assert_eq!(
            decide_consensus_era(
                Some(ERA_A),
                ERA_B,
                true,
                40,
                40,
                Some((40, hash_at_anchor)),
                hash_at_anchor,
            )
            .unwrap(),
            EraDecision::Adopt
        );
        // Even acknowledged, the cutover must be exact.
        assert!(
            decide_consensus_era(
                Some(ERA_A),
                ERA_B,
                true,
                41,
                40,
                Some((40, hash_at_anchor)),
                hash_at_anchor,
            )
            .is_err()
        );

        // Era mixing: a different era over EXISTING engine state is always fatal,
        // acknowledged or not.
        assert!(
            decide_consensus_era(
                Some(ERA_A),
                ERA_B,
                false,
                40,
                40,
                Some((40, hash_at_anchor)),
                hash_at_anchor,
            )
            .is_err()
        );

        // Legacy instance from before era tracking: adopt regardless of tip.
        assert_eq!(
            decide_consensus_era(None, ERA_A, false, 500, 0, None, hash_at_anchor).unwrap(),
            EraDecision::Adopt
        );
    }

    #[test]
    fn acknowledge_fork_parses_and_rejects_garbage() {
        assert_eq!(parse_acknowledge_fork(&None).unwrap(), None);
        let hash = alloy::primitives::B256::repeat_byte(0xAB);
        let parsed = parse_acknowledge_fork(&Some(format!("42:{hash}"))).unwrap();
        assert_eq!(parsed, Some((42, hash)));
        assert!(parse_acknowledge_fork(&Some("42".to_string())).is_err());
        assert!(parse_acknowledge_fork(&Some("x:0xab".to_string())).is_err());
        assert!(parse_acknowledge_fork(&Some("42:nothex".to_string())).is_err());
    }

    #[test]
    fn rollback_requires_acknowledgment_over_consensus_state() {
        assert!(check_rollback_acknowledged(true, false).is_err());
        check_rollback_acknowledged(true, true).unwrap();
        check_rollback_acknowledged(false, false).unwrap();
        check_rollback_acknowledged(false, true).unwrap();
    }
    /// Deterministic key material for configuration tests, in the config's own hex
    /// entry format.
    fn test_validator(seed: u8, port: u16) -> (String, String, String) {
        use commonware_codec::{DecodeExt as _, Encode as _};
        use commonware_cryptography::Signer as _;
        let network = ed25519::PrivateKey::decode([seed; 32].as_slice()).expect("seed");
        // Scalars must be canonical; small seed bytes are.
        let bls = group::Private::decode(
            [
                0u8,
                seed.max(1),
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                1,
            ]
            .as_slice(),
        )
        .expect("canonical scalar");
        let bls_public =
            commonware_cryptography::bls12381::primitives::ops::compute_public::<MinPk>(&bls);
        let entry = format!(
            "{}:{}@127.0.0.1:{port}",
            alloy::hex::encode(network.public_key().encode()),
            alloy::hex::encode(bls_public.encode()),
        );
        (
            entry,
            alloy::hex::encode(network.encode()),
            alloy::hex::encode(bls.encode()),
        )
    }

    fn config_with(
        validators: Vec<String>,
        committees: Vec<crate::config::CommitteeScheduleEntryConfig>,
        network_key: String,
        bls_key: String,
    ) -> ConsensusConfig {
        ConsensusConfig {
            enabled: true,
            network_key: Some(network_key),
            bls_key: Some(bls_key),
            validators,
            committees,
            ..ConsensusConfig::default()
        }
    }

    #[test]
    fn validators_shorthand_is_a_single_epoch_zero_committee() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        let setup = ConsensusSetup::from_config(
            &config_with(vec![a, b], vec![], a_net, a_bls),
            std::env::temp_dir(),
            6565,
        )
        .expect("valid config");
        assert_eq!(setup.schedule.entries().len(), 1);
        assert_eq!(setup.schedule.entries()[0].activation_epoch, 0);
        assert_eq!(setup.committee.len(), 2);
    }

    #[test]
    fn schedule_entries_resolve_and_union_the_address_book() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        let (c, _, _) = test_validator(3, 4003);
        let committees = vec![
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: 0,
                validators: vec![a.clone(), b.clone()],
                source: Default::default(),
            },
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: 2,
                validators: vec![a.clone(), b.clone(), c.clone()],
                source: Default::default(),
            },
        ];
        let setup = ConsensusSetup::from_config(
            &config_with(vec![], committees, a_net, a_bls),
            std::env::temp_dir(),
            6565,
        )
        .expect("valid config");
        assert_eq!(setup.schedule.entries().len(), 2);
        // The address book carries the epoch-2 joiner so it is dialable early.
        assert_eq!(setup.committee.len(), 3);
    }

    #[test]
    fn a_key_in_no_committee_requires_acknowledgment() {
        let (a, _, _) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        // Validator 3 is configured with its own keys but appears in no committee.
        let (_, outsider_net, outsider_bls) = test_validator(3, 4003);
        let config = config_with(vec![a, b], vec![], outsider_net, outsider_bls);
        let err = ConsensusSetup::from_config(&config, std::env::temp_dir(), 6565)
            .map(|_| ())
            .expect_err("must refuse a non-member without acknowledgment");
        assert!(err.to_string().contains("acknowledge_non_member"));

        let acknowledged = ConsensusConfig {
            acknowledge_non_member: true,
            ..config
        };
        ConsensusSetup::from_config(&acknowledged, std::env::temp_dir(), 6565)
            .map(|_| ())
            .expect("acknowledged follower mode starts");
    }

    #[test]
    fn a_mismatched_bls_pairing_is_refused_loudly() {
        let (a, a_net, _) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        // The schedule lists validator 1's network key with validator 1's BLS key,
        // but this node is (mis)configured with validator 3's signing key.
        let (_, _, wrong_bls) = test_validator(3, 4003);
        let err = ConsensusSetup::from_config(
            &config_with(vec![a, b], vec![], a_net, wrong_bls),
            std::env::temp_dir(),
            6565,
        )
        .map(|_| ())
        .expect_err("a BLS pairing mismatch would silently never vote");
        assert!(err.to_string().contains("never vote"), "got: {err}");
    }

    #[test]
    fn conflicting_member_identities_across_entries_are_refused() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        // Same network key as `b`, different port — two entries disagree about who
        // the validator is.
        let (b_moved, _, _) = test_validator(2, 5002);
        let committees = vec![
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: 0,
                validators: vec![a.clone(), b],
                source: Default::default(),
            },
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: 2,
                validators: vec![a, b_moved],
                source: Default::default(),
            },
        ];
        let err = ConsensusSetup::from_config(
            &config_with(vec![], committees, a_net, a_bls),
            std::env::temp_dir(),
            6565,
        )
        .map(|_| ())
        .expect_err("conflicting identities must be refused");
        assert!(err.to_string().contains("different BLS key or address"));
    }

    #[test]
    fn the_first_schedule_entry_must_activate_at_epoch_zero() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        let committees = vec![crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 3,
            validators: vec![a, b],
            source: Default::default(),
        }];
        let err = ConsensusSetup::from_config(
            &config_with(vec![], committees, a_net, a_bls),
            std::env::temp_dir(),
            6565,
        )
        .map(|_| ())
        .expect_err("a schedule with a hole before its first entry must be refused");
        assert!(err.to_string().contains("committee schedule"), "got: {err}");
    }

    /// One committee entry at epoch 0 plus the registry flip at `flip`.
    fn committees_with_flip(
        members: Vec<String>,
        flip: u64,
    ) -> Vec<crate::config::CommitteeScheduleEntryConfig> {
        vec![
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: 0,
                validators: members,
                source: Default::default(),
            },
            crate::config::CommitteeScheduleEntryConfig {
                activation_epoch: flip,
                validators: vec![],
                source: crate::config::CommitteeEntrySource::Registry,
            },
        ]
    }

    #[test]
    fn registry_mode_validation_covers_the_matrix() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        let registry_address = Some(alloy::primitives::Address::repeat_byte(0x42));
        let base = |mode,
                    committees: Option<Vec<crate::config::CommitteeScheduleEntryConfig>>,
                    address| ConsensusConfig {
            registry_mode: mode,
            registry_address: address,
            ..config_with(
                if committees.is_none() {
                    vec![a.clone(), b.clone()]
                } else {
                    vec![]
                },
                committees.unwrap_or_default(),
                a_net.clone(),
                a_bls.clone(),
            )
        };
        let setup = |config: &ConsensusConfig| {
            ConsensusSetup::from_config(config, std::env::temp_dir(), 6565).map(|_| ())
        };

        // Schedule mode: an address that would be silently ignored is refused.
        setup(&base(RegistryMode::Schedule, None, None)).expect("plain schedule mode");
        let err = setup(&base(RegistryMode::Schedule, None, registry_address)).unwrap_err();
        assert!(err.to_string().contains("registry_address"), "got: {err}");

        // Shadow mode: the address is required; a flip entry is refused.
        setup(&base(RegistryMode::Shadow, None, registry_address)).expect("shadow mode");
        let err = setup(&base(RegistryMode::Shadow, None, None)).unwrap_err();
        assert!(err.to_string().contains("registry_address"), "got: {err}");
        let flip = committees_with_flip(vec![a.clone(), b.clone()], 4);
        let err = setup(&base(
            RegistryMode::Shadow,
            Some(flip.clone()),
            registry_address,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("config_shadow"), "got: {err}");

        // Config-shadow mode: needs both the address and exactly one flip entry.
        setup(&base(
            RegistryMode::ConfigShadow,
            Some(flip.clone()),
            registry_address,
        ))
        .expect("config_shadow mode");
        let err = setup(&base(RegistryMode::ConfigShadow, None, registry_address)).unwrap_err();
        assert!(err.to_string().contains("source: registry"), "got: {err}");
        let mut two_flips = flip.clone();
        two_flips.push(crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 9,
            validators: vec![],
            source: crate::config::CommitteeEntrySource::Registry,
        });
        let err = setup(&base(
            RegistryMode::ConfigShadow,
            Some(two_flips),
            registry_address,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("exactly one"), "got: {err}");

        // A flip entry listing validators, or claiming epoch 0, is refused.
        let mut listing = committees_with_flip(vec![a.clone(), b.clone()], 4);
        listing[1].validators = vec![a.clone()];
        let err = setup(&base(
            RegistryMode::ConfigShadow,
            Some(listing),
            registry_address,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("must be empty"), "got: {err}");
        let at_zero = committees_with_flip(vec![a.clone(), b.clone()], 0);
        let err = setup(&base(
            RegistryMode::ConfigShadow,
            Some(at_zero),
            registry_address,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("epoch 0"), "got: {err}");
    }

    #[test]
    fn config_shadow_mode_resolves_the_flip_and_mirror_entries_do_not_override() {
        let (a, a_net, a_bls) = test_validator(1, 4001);
        let (b, _, _) = test_validator(2, 4002);
        let mut committees = committees_with_flip(vec![a.clone(), b.clone()], 4);
        // A mirror entry after the flip: config tracking a registry rotation.
        committees.push(crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 7,
            validators: vec![a.clone(), b.clone()],
            source: Default::default(),
        });
        let config = ConsensusConfig {
            registry_mode: RegistryMode::ConfigShadow,
            registry_address: Some(alloy::primitives::Address::repeat_byte(0x42)),
            ..config_with(vec![], committees, a_net, a_bls)
        };
        let setup =
            ConsensusSetup::from_config(&config, std::env::temp_dir(), 6565).expect("valid");
        let registry = setup.registry.expect("registry participates");
        assert_eq!(registry.flip_epoch, Some(4));
        assert_eq!(registry.chain_id, 6565);
        // The provider's source carries the flip: epoch 3 is config-settled;
        // everything at or after the flip — including the epoch the mirror
        // entry names — waits for a derivation (mirrors never override).
        use zksync_os_consensus_core::types::Epoch;
        assert!(setup.provider.settled_for(Epoch::new(3)));
        assert!(!setup.provider.settled_for(Epoch::new(4)));
        assert!(!setup.provider.settled_for(Epoch::new(7)));
        // Shadow mode has no flip: everything stays config-settled.
        let shadow = ConsensusSetup::from_config(
            &ConsensusConfig {
                registry_mode: RegistryMode::Shadow,
                registry_address: Some(alloy::primitives::Address::repeat_byte(0x42)),
                ..config_with(
                    vec![a.clone(), b.clone()],
                    vec![],
                    config.network_key.clone().unwrap(),
                    config.bls_key.clone().unwrap(),
                )
            },
            std::env::temp_dir(),
            6565,
        )
        .expect("valid shadow config");
        assert!(shadow.registry.expect("participates").flip_epoch.is_none());
        assert!(shadow.provider.settled_for(Epoch::new(1_000)));
    }
}
