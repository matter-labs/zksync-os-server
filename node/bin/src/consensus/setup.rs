//! Parsing and validating the consensus configuration into a runnable setup.

use anyhow::Context as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519;
use commonware_utils::TryCollect as _;
use commonware_utils::ordered::BiMap;
use commonware_utils::union_unique;
use smart_config::value::ExposeSecret as _;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use zksync_os_consensus_core::types::SchemeProvider;
use zksync_os_consensus_core::{CommitteeSchedule, CommitteeSource, ScheduleEntry};

use crate::config::{CommitteeEntrySource, ConsensusConfig, RegistryMode};

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
    pub max_message_size: NonZeroU32,
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
                .context("`consensus.network_key` is required")?
                .expose_secret(),
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
                        .context("`consensus.bls_key` is required for validators")?
                        .expose_secret(),
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

        let base_namespace = super::namespace(config.protocol_version);
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
