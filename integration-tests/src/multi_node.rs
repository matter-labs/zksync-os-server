//! A multi-validator BFT cluster of real in-process nodes over one shared L1.
//!
//! Every validator is a full node — its own runtime, databases, RPC — configured into
//! one consensus committee. Validators must launch *concurrently*: a lone validator has
//! no quorum, produces no blocks, and would therefore never see the initial deposit
//! that node startup waits for.

use crate::l1_proxy::SeverableL1Proxy;
use crate::test_config::{build_node_config, disable_prover_input_generation};
use crate::utils::LockedPort;
use crate::{ChainLayout, PROTOCOL_VERSION, StoppedTester, Tester};
use anyhow::Context as _;
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519;
use futures::future::try_join_all;
use zksync_os_server::config::{CommitteeScheduleEntryConfig, Config, ConsensusRole};
use zksync_os_types::NodeRole;

/// One validator's key set, generated fresh per cluster.
struct ValidatorKeys {
    network: ed25519::PrivateKey,
    bls: commonware_cryptography::bls12381::primitives::group::Private,
    committee_entry_keys: String,
}

fn generate_validator_keys() -> ValidatorKeys {
    let mut rng = rand08::rngs::OsRng;
    let mut seed = [0u8; 32];
    rand08::RngCore::fill_bytes(&mut rng, &mut seed);
    let network =
        ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes are a valid key");
    let (bls, bls_public) = ops::keypair::<_, MinPk>(&mut rng);
    let committee_entry_keys = format!(
        "{}:{}",
        alloy::hex::encode(network.public_key().encode()),
        alloy::hex::encode(bls_public.encode()),
    );
    ValidatorKeys {
        network,
        bls,
        committee_entry_keys,
    }
}

/// A fresh consensus identity plus a reserved p2p port, for tests that build a
/// committee by hand — e.g. a scheduled cutover, which arms consensus on nodes
/// the multi-node harness does not manage. Keep the seat alive for the test's
/// duration: dropping it releases the reserved port.
pub struct CommitteeSeat {
    keys: ValidatorKeys,
    port: crate::utils::LockedPort,
}

impl CommitteeSeat {
    pub async fn reserve() -> anyhow::Result<Self> {
        Ok(Self {
            keys: generate_validator_keys(),
            port: crate::utils::LockedPort::acquire_unused().await?,
        })
    }

    /// This seat's entry for every node's `consensus.validators` list.
    pub fn committee_entry(&self) -> String {
        format!(
            "{}@127.0.0.1:{}",
            self.keys.committee_entry_keys, self.port.port
        )
    }

    /// Arms consensus on `config` as this seat, over the given committee.
    pub fn arm_consensus(&self, config: &mut Config, committee: Vec<String>, genesis_height: u64) {
        config.consensus_config.enabled = true;
        config.consensus_config.network_key =
            Some(alloy::hex::encode(self.keys.network.encode()).into());
        config.consensus_config.bls_key = Some(alloy::hex::encode(self.keys.bls.encode()).into());
        config.consensus_config.listen_address = format!("127.0.0.1:{}", self.port.port);
        config.consensus_config.validators = committee;
        config.consensus_config.allow_private_ips = true;
        config.consensus_config.genesis_height = genesis_height;
    }
}

/// One validator's deviation from the shared committee schedule: the validator
/// index and the full schedule its config carries instead (activation epoch →
/// validator indices per entry).
pub type ScheduleOverride = (usize, Vec<(u64, Vec<usize>)>);

/// One validator slot: a running node, a stopped one (restartable on the same state
/// and keys), or the momentary in-between while a transition is in flight.
/// (Test-harness type — the size imbalance between variants is irrelevant here.)
#[allow(clippy::large_enum_variant)]
enum Validator {
    Running(Tester),
    Stopped(StoppedTester),
    Transitioning,
}

pub struct MultiNodeTester {
    validators: Vec<Validator>,
    /// Reservations for the consensus listen ports, held for the cluster's lifetime so
    /// a stopped validator can rebind the same address when it restarts.
    consensus_ports: Vec<LockedPort>,
    /// Every node's committee entry (`<ed25519>:<bls>@<addr>`) — observers included,
    /// whose entries exist for tests that *promote* them into a future committee.
    committee_entries: Vec<String>,
    /// Every node's BLS signing key (hex). An observer's key is generated but not
    /// configured; promotion tests hand it to the node at the role flip.
    bls_keys: Vec<String>,
}

impl MultiNodeTester {
    /// Starts `num_validators` validators as one committee. Exactly one node (the first)
    /// runs the batcher; all serve RPC.
    /// Like [`Self::start`], with one shared mutation applied to every
    /// validator's config after the standard committee wiring (chain-level
    /// constants must stay identical across the committee).
    pub async fn start_with_config_overrides(
        num_validators: usize,
        overrides: impl Fn(&mut Config) + Clone + Send + 'static,
    ) -> anyhow::Result<Self> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        Self::start_inner_with(num_validators, chain_layout, l1, overrides).await
    }

    pub async fn start(num_validators: usize) -> anyhow::Result<Self> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        Self::start_inner(num_validators, chain_layout, l1).await
    }

    /// Like [`Self::start`], with the committee acting as the batch-verification
    /// (2FA) set: every validator meshes on the zks network with a stable
    /// identity, carries its own verifier signing key, and co-signs the
    /// settler's batch commitments against its own finalized chain. The settler
    /// collects `threshold` signatures before committing to L1 — it never
    /// co-signs its own batches, so `threshold` must be reachable from the
    /// standbys alone (`threshold <= n - 1 - f`).
    pub async fn start_with_batch_verification(
        num_validators: usize,
        threshold: u64,
    ) -> anyhow::Result<Self> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;

        // Every validator's zks-network identity, decided up front so each
        // node's boot list can name all the others. The port reservations are
        // released just before launch; the nodes then bind those concrete
        // ports themselves (the unlocked window is tolerated the same way
        // single-node tests tolerate port churn — a loud launch failure,
        // never silent misbehavior).
        let mut network_ports = Vec::with_capacity(num_validators);
        for _ in 0..num_validators {
            network_ports.push(crate::utils::LockedPort::acquire_unused().await?);
        }
        let network_secrets: Vec<_> = (0..num_validators)
            .map(|_| zksync_os_network::rng_secret_key())
            .collect();
        let node_records: Vec<zksync_os_network::NodeRecord> = network_secrets
            .iter()
            .zip(&network_ports)
            .map(|(secret, port)| {
                zksync_os_network::NodeRecord::from_secret_key(
                    std::net::SocketAddr::new(
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                        port.port,
                    ),
                    secret,
                )
            })
            .collect();
        let network_port_numbers: Vec<u16> = network_ports.iter().map(|port| port.port).collect();
        // Release the reservations before launching: the nodes bind the
        // concrete ports written into their configs (a pre-set network
        // identity survives `bind_runtime_config`), and a restarting
        // validator rebinds its configured port after waiting for the
        // previous incarnation's listener to be released.
        drop(network_ports);

        // Per-validator verifier identities; the allow-list every settler
        // checks results against is the union.
        let verifier_keys: Vec<String> = (0..num_validators)
            .map(|index| format!("0x{}", alloy::hex::encode([0xB1 + index as u8; 32])))
            .collect();
        let verifier_addresses: Vec<String> = verifier_keys
            .iter()
            .map(|key| {
                <alloy::signers::local::PrivateKeySigner as std::str::FromStr>::from_str(key)
                    .expect("static test key")
                    .address()
                    .to_string()
            })
            .collect();

        Self::start_inner_indexed(num_validators, chain_layout, l1, move |index, config| {
            tracing::info!(
                index,
                port = network_port_numbers[index],
                boot_nodes = ?node_records
                    .iter()
                    .enumerate()
                    .filter(|(peer, _)| *peer != index)
                    .map(|(_, record)| record.to_string())
                    .collect::<Vec<_>>(),
                "meshing validator on the zks network",
            );
            config.network_config.enabled = true;
            config.network_config.port = network_port_numbers[index];
            config.network_config.secret_key = Some(network_secrets[index]);
            // Dial in one direction only (toward higher indices): both ends
            // dialing each other makes reth's duplicate-session teardown and
            // the short reconnect backoffs churn sessions forever, and verify
            // responses lose the race against session lifetime. One stable
            // session per pair serves both directions.
            config.network_config.boot_nodes = node_records
                .iter()
                .enumerate()
                .filter(|(peer, _)| *peer > index)
                .map(|(_, record)| (*record).into())
                .collect();
            let verification = &mut config.batch_verification_config;
            verification.server_enabled = true;
            verification.client_enabled = true;
            verification.threshold = threshold;
            verification.accepted_signers = verifier_addresses.clone();
            verification.signing_key = verifier_keys[index].clone().into();
        })
        .await
    }

    /// Like [`Self::start`], but every validator reaches L1 through a
    /// [`SeverableL1Proxy`] the test controls — sever it to emulate a shared L1
    /// RPC provider outage for the whole committee, restore it to end the outage.
    /// The returned tester's own L1 helpers keep a direct anvil connection, so
    /// tests can observe L1 while the committee cannot.
    pub async fn start_with_severable_l1(
        num_validators: usize,
    ) -> anyhow::Result<(Self, SeverableL1Proxy)> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        let proxy = SeverableL1Proxy::start(&l1.address).await?;
        // Nodes derive their L1 RPC URL from the `AnvilL1` handle they are launched
        // with; substituting the address routes every validator through the proxy
        // (the handle's own provider object stays directly connected).
        let mut proxied_l1 = l1.clone();
        proxied_l1.address = proxy.url();
        let tester = Self::start_inner(num_validators, chain_layout, proxied_l1).await?;
        Ok((tester, proxy))
    }

    /// Like [`Self::start`], but validator `laggard` reaches L1 through its own
    /// [`SeverableL1Proxy`] while every other validator — and the testers' own
    /// L1 helpers — stay directly connected. Severing it freezes one
    /// validator's *view* of L1 while the chain's L1 keeps moving: the setup
    /// for L1-view-divergence scenarios (a leader includes an L1 input the
    /// laggard has not observed yet).
    pub async fn start_with_lagging_l1(
        num_validators: usize,
        laggard: usize,
    ) -> anyhow::Result<(Self, SeverableL1Proxy)> {
        assert!(laggard < num_validators);
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        let proxy = SeverableL1Proxy::start(&l1.address).await?;
        let mut lagged = l1.clone();
        lagged.address = proxy.url();
        let tester = Self::start_inner_indexed_l1(
            num_validators,
            chain_layout,
            move |index| {
                if index == laggard {
                    lagged.clone()
                } else {
                    l1.clone()
                }
            },
            |_, _| {},
        )
        .await?;
        Ok((tester, proxy))
    }

    /// The migration cutover: a committee takes over a chain that a single
    /// sequencer produced. `source` is the drained (stopped) sequencer node; every
    /// validator starts on a copy of its chain databases — the snapshot-distribution
    /// step of a real migration — with consensus anchored at the drained tip.
    pub async fn migrate_from(
        source: &StoppedTester,
        num_validators: usize,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );

        // The anchor is the drained chain's exact write-ahead-log tip. An RPC height
        // read before the stop is not reliable (the sequencer keeps producing until
        // the process winds down), so read it from the stopped node's database —
        // the same thing a migration operator does after draining.
        let source_rocks = source.config.general_config.rocks_db_path.clone();
        crate::wait_for_rocksdb_locks_released(&source_rocks).await?;
        let anchor_height = {
            use zksync_os_storage_api::ReadReplay as _;
            let wal = zksync_os_storage::db::BlockReplayStorage::new_without_genesis(
                &source_rocks.join(zksync_os_server::BLOCK_REPLAY_WAL_DB_NAME),
                source
                    .config
                    .genesis_config
                    .chain_id
                    .context("test node genesis config must set chain_id")?,
            );
            wal.latest_record()
        };

        let keys: Vec<ValidatorKeys> = (0..num_validators)
            .map(|_| generate_validator_keys())
            .collect();
        let mut consensus_ports = Vec::with_capacity(num_validators);
        for _ in 0..num_validators {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        let committee: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();

        let l1 = source.l1.clone();
        let chain_layout = source.chain_layout;
        // Chain constants are committee-wide and pinned by verification; keeping the
        // pre-migration chain's fee collector is the realistic choice (any uniform
        // value would verify — fees flow wherever the committee configures).
        let fee_collector = source.config.sequencer_config.fee_collector_address;
        let launches =
            keys.iter()
                .zip(&consensus_ports)
                .enumerate()
                .map(|(index, (keys, consensus_port))| {
                    let l1 = l1.clone();
                    let committee = committee.clone();
                    let network_key = alloy::hex::encode(keys.network.encode());
                    let bls_key = alloy::hex::encode(keys.bls.encode());
                    let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                    let source_rocks = source_rocks.clone();
                    async move {
                        let mut config = build_node_config(&l1, chain_layout, false).await?;
                        disable_prover_input_generation(&mut config);
                        config.general_config.node_role = NodeRole::MainNode;
                        config.sequencer_config.fee_collector_address = fee_collector;
                        // Exactly one batcher, as before the migration.
                        config.batcher_config.enabled = index == 0;
                        config.consensus_config.enabled = true;
                        config.consensus_config.network_key = Some(network_key.into());
                        config.consensus_config.bls_key = Some(bls_key.into());
                        config.consensus_config.listen_address = listen_address;
                        config.consensus_config.validators = committee;
                        config.consensus_config.allow_private_ips = true;
                        config.consensus_config.genesis_height = anchor_height;
                        Tester::launch_with_seeded_state(l1, chain_layout, config, &source_rocks)
                            .await
                            .with_context(|| format!("failed to launch migrated validator {index}"))
                    }
                });
        let nodes = try_join_all(launches).await?;
        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
            committee_entries: committee,
            bls_keys: keys
                .iter()
                .map(|keys| alloy::hex::encode(keys.bls.encode()))
                .collect(),
        })
    }

    /// Starts `num_validators` nodes over a committee *schedule*: every node runs
    /// from genesis, but only the validators scheduled into an epoch's committee
    /// vote in it — the rest follow (the deploy-first-activate-later order of a
    /// real committee change). `schedule` entries are `(activation_epoch,
    /// validator indices)`; `epoch_length` is in blocks and should be small enough
    /// for the test to cross boundaries quickly.
    pub async fn start_with_schedule(
        num_validators: usize,
        schedule: &[(u64, Vec<usize>)],
        epoch_length: u64,
    ) -> anyhow::Result<Self> {
        Self::start_with_schedule_and_overrides(num_validators, schedule, epoch_length, &[]).await
    }

    /// Like [`Self::start_with_schedule`], but every validator also runs the
    /// on-chain registry in *shadow* mode against `registry_address`: committees
    /// still come from the config schedule, and each node additionally derives
    /// the would-be committee from the registry contract's storage at every
    /// epoch's lookahead boundary, reporting match/mismatch in
    /// `/status.consensus.registry`. The address is configuration, so tests
    /// precompute the deployment address and deploy after startup — an
    /// undeployed registry derives quietly as "nothing scheduled".
    pub async fn start_with_shadow_registry(
        num_validators: usize,
        epoch_length: u64,
        registry_address: alloy::primitives::Address,
    ) -> anyhow::Result<Self> {
        let everyone = vec![(0, (0..num_validators).collect::<Vec<_>>())];
        Self::start_with_schedule_inner(
            num_validators,
            &everyone,
            epoch_length,
            &[],
            Some(registry_address),
        )
        .await
    }

    /// Like [`Self::start_with_schedule`], but the listed validators run their own
    /// (wrong) view of the schedule — the operator-error scenarios: a validator
    /// whose deployed config is missing the newest committee entry.
    pub async fn start_with_schedule_and_overrides(
        num_validators: usize,
        schedule: &[(u64, Vec<usize>)],
        epoch_length: u64,
        schedule_overrides: &[ScheduleOverride],
    ) -> anyhow::Result<Self> {
        Self::start_with_schedule_inner(
            num_validators,
            schedule,
            epoch_length,
            schedule_overrides,
            None,
        )
        .await
    }

    async fn start_with_schedule_inner(
        num_validators: usize,
        schedule: &[(u64, Vec<usize>)],
        epoch_length: u64,
        schedule_overrides: &[ScheduleOverride],
        shadow_registry: Option<alloy::primitives::Address>,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;

        let keys: Vec<ValidatorKeys> = (0..num_validators)
            .map(|_| generate_validator_keys())
            .collect();
        let fee_collector = alloy::primitives::Address::random();
        let mut consensus_ports = Vec::with_capacity(num_validators);
        for _ in 0..num_validators {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        let committee: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();
        let to_entries = |spec: &[(u64, Vec<usize>)]| -> Vec<CommitteeScheduleEntryConfig> {
            spec.iter()
                .map(|(activation_epoch, indices)| CommitteeScheduleEntryConfig {
                    activation_epoch: *activation_epoch,
                    validators: indices.iter().map(|&i| committee[i].clone()).collect(),
                    source: Default::default(),
                })
                .collect()
        };
        let shared_entries = to_entries(schedule);

        let launches =
            keys.iter()
                .zip(&consensus_ports)
                .enumerate()
                .map(|(index, (keys, consensus_port))| {
                    let l1 = l1.clone();
                    let entries = schedule_overrides
                        .iter()
                        .find(|(overridden, _)| *overridden == index)
                        .map(|(_, spec)| to_entries(spec))
                        .unwrap_or_else(|| shared_entries.clone());
                    let network_key = alloy::hex::encode(keys.network.encode());
                    let bls_key = alloy::hex::encode(keys.bls.encode());
                    let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                    async move {
                        let mut config = build_node_config(&l1, chain_layout, false).await?;
                        disable_prover_input_generation(&mut config);
                        config.general_config.node_role = NodeRole::MainNode;
                        config.sequencer_config.fee_collector_address = fee_collector;
                        config.batcher_config.enabled = index == 0;
                        config.consensus_config.enabled = true;
                        config.consensus_config.network_key = Some(network_key.into());
                        config.consensus_config.bls_key = Some(bls_key.into());
                        config.consensus_config.listen_address = listen_address;
                        config.consensus_config.committees = entries;
                        config.consensus_config.epoch_length = epoch_length;
                        config.consensus_config.allow_private_ips = true;
                        if let Some(registry_address) = shadow_registry {
                            config.consensus_config.registry_mode =
                                zksync_os_server::config::RegistryMode::Shadow;
                            config.consensus_config.registry_address = Some(registry_address);
                        }
                        Tester::launch_with_new_runtime(l1, chain_layout, config)
                            .await
                            .with_context(|| format!("failed to launch validator {index}"))
                    }
                });
        let nodes = try_join_all(launches).await?;
        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
            committee_entries: committee,
            bls_keys: keys
                .iter()
                .map(|keys| alloy::hex::encode(keys.bls.encode()))
                .collect(),
        })
    }

    /// Starts `num_validators` validators plus `num_observers` non-voting
    /// observers on the same consensus network. Observers take the indices after
    /// the validators (`node(num_validators)` is the first observer) and slot into
    /// the same lifecycle machinery (`node`, waits, stop/start). Launch is
    /// two-phase on purpose: an observer forwards its RPC-received transactions to
    /// the validators' RPCs and connects to them at startup, so the validators
    /// must be up first.
    pub async fn start_with_observers(
        num_validators: usize,
        num_observers: usize,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;

        let total = num_validators + num_observers;
        // Observers reuse the key generator; their BLS half is simply never
        // configured (an observer holds no signing key).
        let keys: Vec<ValidatorKeys> = (0..total).map(|_| generate_validator_keys()).collect();
        let fee_collector = alloy::primitives::Address::random();
        let mut consensus_ports = Vec::with_capacity(total);
        for _ in 0..total {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        // Committee-style entries for every node: the validators' feed the config;
        // an observer's is the entry a future committee would list (promotion
        // material, stored on the tester).
        let all_entries: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();
        let committee: Vec<String> = all_entries[..num_validators].to_vec();
        // `consensus.observers` entries carry only the network identity.
        let observer_entries: Vec<String> = keys[num_validators..]
            .iter()
            .zip(&consensus_ports[num_validators..])
            .map(|(keys, port)| {
                let network_hex = keys
                    .committee_entry_keys
                    .split(':')
                    .next()
                    .expect("committee entry keys are `<net>:<bls>`");
                format!("{network_hex}@127.0.0.1:{}", port.port)
            })
            .collect();

        // Phase 1: the committee.
        let launches = keys[..num_validators]
            .iter()
            .zip(&consensus_ports)
            .enumerate()
            .map(|(index, (keys, consensus_port))| {
                let l1 = l1.clone();
                let committee = committee.clone();
                let observer_entries = observer_entries.clone();
                let network_key = alloy::hex::encode(keys.network.encode());
                let bls_key = alloy::hex::encode(keys.bls.encode());
                let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                async move {
                    let mut config = build_node_config(&l1, chain_layout, false).await?;
                    disable_prover_input_generation(&mut config);
                    config.general_config.node_role = NodeRole::MainNode;
                    config.sequencer_config.fee_collector_address = fee_collector;
                    config.batcher_config.enabled = index == 0;
                    config.consensus_config.enabled = true;
                    config.consensus_config.network_key = Some(network_key.into());
                    config.consensus_config.bls_key = Some(bls_key.into());
                    config.consensus_config.listen_address = listen_address;
                    config.consensus_config.validators = committee;
                    config.consensus_config.observers = observer_entries;
                    config.consensus_config.allow_private_ips = true;
                    Tester::launch_with_new_runtime(l1, chain_layout, config)
                        .await
                        .with_context(|| format!("failed to launch validator {index}"))
                }
            });
        let mut nodes = try_join_all(launches).await?;

        // Phase 2: the observers, forwarding to the now-live validator RPCs.
        let forward_urls: Vec<String> = nodes
            .iter()
            .map(|node| node.l2_rpc_url().to_string())
            .collect();
        for (offset, (keys, consensus_port)) in keys[num_validators..]
            .iter()
            .zip(&consensus_ports[num_validators..])
            .enumerate()
        {
            let network_key = alloy::hex::encode(keys.network.encode());
            let listen_address = format!("127.0.0.1:{}", consensus_port.port);
            let mut config = build_node_config(&l1, chain_layout, false).await?;
            disable_prover_input_generation(&mut config);
            config.general_config.node_role = NodeRole::MainNode;
            config.sequencer_config.fee_collector_address = fee_collector;
            config.batcher_config.enabled = false;
            config.consensus_config.enabled = true;
            config.consensus_config.role = ConsensusRole::Observer;
            config.consensus_config.network_key = Some(network_key.into());
            config.consensus_config.listen_address = listen_address;
            config.consensus_config.validators = committee.clone();
            config.consensus_config.observers = observer_entries.clone();
            config.consensus_config.tx_forward_rpc_urls = forward_urls.clone();
            config.consensus_config.allow_private_ips = true;
            let node = Tester::launch_with_new_runtime(l1.clone(), chain_layout, config)
                .await
                .with_context(|| format!("failed to launch observer {offset}"))?;
            nodes.push(node);
        }

        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
            committee_entries: all_entries,
            bls_keys: keys
                .iter()
                .map(|keys| alloy::hex::encode(keys.bls.encode()))
                .collect(),
        })
    }

    /// [`Self::start_with_observers`] over a committee *schedule* — the promotion
    /// shape: observers that a later schedule entry will make validators. Observer
    /// indices follow the validator indices. `schedule` entries index into the
    /// validators only; promoting an observer later means restarting nodes with an
    /// appended entry that lists the observer's [`Self::committee_entry`].
    pub async fn start_with_schedule_and_observers(
        num_validators: usize,
        num_observers: usize,
        schedule: &[(u64, Vec<usize>)],
        epoch_length: u64,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;

        let total = num_validators + num_observers;
        let keys: Vec<ValidatorKeys> = (0..total).map(|_| generate_validator_keys()).collect();
        let fee_collector = alloy::primitives::Address::random();
        let mut consensus_ports = Vec::with_capacity(total);
        for _ in 0..total {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        let all_entries: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();
        let entries: Vec<CommitteeScheduleEntryConfig> = schedule
            .iter()
            .map(|(activation_epoch, indices)| CommitteeScheduleEntryConfig {
                activation_epoch: *activation_epoch,
                validators: indices.iter().map(|&i| all_entries[i].clone()).collect(),
                source: Default::default(),
            })
            .collect();
        let observer_entries: Vec<String> = keys[num_validators..]
            .iter()
            .zip(&consensus_ports[num_validators..])
            .map(|(keys, port)| {
                let network_hex = keys
                    .committee_entry_keys
                    .split(':')
                    .next()
                    .expect("committee entry keys are `<net>:<bls>`");
                format!("{network_hex}@127.0.0.1:{}", port.port)
            })
            .collect();

        // Phase 1: the committee.
        let launches = keys[..num_validators]
            .iter()
            .zip(&consensus_ports)
            .enumerate()
            .map(|(index, (keys, consensus_port))| {
                let l1 = l1.clone();
                let entries = entries.clone();
                let observer_entries = observer_entries.clone();
                let network_key = alloy::hex::encode(keys.network.encode());
                let bls_key = alloy::hex::encode(keys.bls.encode());
                let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                async move {
                    let mut config = build_node_config(&l1, chain_layout, false).await?;
                    disable_prover_input_generation(&mut config);
                    config.general_config.node_role = NodeRole::MainNode;
                    config.sequencer_config.fee_collector_address = fee_collector;
                    config.batcher_config.enabled = index == 0;
                    config.consensus_config.enabled = true;
                    config.consensus_config.network_key = Some(network_key.into());
                    config.consensus_config.bls_key = Some(bls_key.into());
                    config.consensus_config.listen_address = listen_address;
                    config.consensus_config.committees = entries;
                    config.consensus_config.epoch_length = epoch_length;
                    config.consensus_config.observers = observer_entries;
                    config.consensus_config.allow_private_ips = true;
                    Tester::launch_with_new_runtime(l1, chain_layout, config)
                        .await
                        .with_context(|| format!("failed to launch validator {index}"))
                }
            });
        let mut nodes = try_join_all(launches).await?;

        // Phase 2: the observers, forwarding to the now-live validator RPCs.
        let forward_urls: Vec<String> = nodes
            .iter()
            .map(|node| node.l2_rpc_url().to_string())
            .collect();
        for (offset, (keys, consensus_port)) in keys[num_validators..]
            .iter()
            .zip(&consensus_ports[num_validators..])
            .enumerate()
        {
            let network_key = alloy::hex::encode(keys.network.encode());
            let listen_address = format!("127.0.0.1:{}", consensus_port.port);
            let mut config = build_node_config(&l1, chain_layout, false).await?;
            disable_prover_input_generation(&mut config);
            config.general_config.node_role = NodeRole::MainNode;
            config.sequencer_config.fee_collector_address = fee_collector;
            config.batcher_config.enabled = false;
            config.consensus_config.enabled = true;
            config.consensus_config.role = ConsensusRole::Observer;
            config.consensus_config.network_key = Some(network_key.into());
            config.consensus_config.listen_address = listen_address;
            config.consensus_config.committees = entries.clone();
            config.consensus_config.epoch_length = epoch_length;
            config.consensus_config.observers = observer_entries.clone();
            config.consensus_config.tx_forward_rpc_urls = forward_urls.clone();
            config.consensus_config.allow_private_ips = true;
            let node = Tester::launch_with_new_runtime(l1.clone(), chain_layout, config)
                .await
                .with_context(|| format!("failed to launch observer {offset}"))?;
            nodes.push(node);
        }

        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
            committee_entries: all_entries,
            bls_keys: keys
                .iter()
                .map(|keys| alloy::hex::encode(keys.bls.encode()))
                .collect(),
        })
    }

    async fn start_inner(
        num_validators: usize,
        chain_layout: ChainLayout<'static>,
        l1: crate::AnvilL1,
    ) -> anyhow::Result<Self> {
        Self::start_inner_with(num_validators, chain_layout, l1, |_| {}).await
    }

    async fn start_inner_with(
        num_validators: usize,
        chain_layout: ChainLayout<'static>,
        l1: crate::AnvilL1,
        overrides: impl Fn(&mut Config) + Clone + Send + 'static,
    ) -> anyhow::Result<Self> {
        Self::start_inner_indexed(num_validators, chain_layout, l1, move |_, config| {
            overrides(config)
        })
        .await
    }

    /// Like [`Self::start_inner_with`], with the validator index available to the
    /// mutation — for per-validator facts (own signing keys, network identities).
    async fn start_inner_indexed(
        num_validators: usize,
        chain_layout: ChainLayout<'static>,
        l1: crate::AnvilL1,
        overrides: impl Fn(usize, &mut Config) + Clone + Send + 'static,
    ) -> anyhow::Result<Self> {
        Self::start_inner_indexed_l1(num_validators, chain_layout, move |_| l1.clone(), overrides)
            .await
    }

    /// Like [`Self::start_inner_indexed`], with a per-validator L1 handle — the
    /// seam for tests that route *individual* validators through their own
    /// proxy (config overrides cannot do this: `bind_runtime_config`
    /// re-derives the RPC URL from the handle's address after any hook runs).
    async fn start_inner_indexed_l1(
        num_validators: usize,
        chain_layout: ChainLayout<'static>,
        l1_for: impl Fn(usize) -> crate::AnvilL1 + Clone + Send + 'static,
        overrides: impl Fn(usize, &mut Config) + Clone + Send + 'static,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );

        let keys: Vec<ValidatorKeys> = (0..num_validators)
            .map(|_| generate_validator_keys())
            .collect();
        // Chain-level constants must be configured identically across the committee
        // (verification pins them); the per-node defaults randomize this one.
        let fee_collector = alloy::primitives::Address::random();
        // Consensus listen ports are allocated here and stay locked until all nodes are
        // up (the node harness allocates its own RPC/network ports separately).
        let mut consensus_ports = Vec::with_capacity(num_validators);
        for _ in 0..num_validators {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        let committee: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();

        let launches =
            keys.iter()
                .zip(&consensus_ports)
                .enumerate()
                .map(|(index, (keys, consensus_port))| {
                    let l1 = l1_for(index);
                    let committee = committee.clone();
                    let overrides = overrides.clone();
                    let network_key = alloy::hex::encode(keys.network.encode());
                    let bls_key = alloy::hex::encode(keys.bls.encode());
                    let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                    async move {
                        let mut config = build_node_config(&l1, chain_layout, false).await?;
                        disable_prover_input_generation(&mut config);
                        config.general_config.node_role = NodeRole::MainNode;
                        config.sequencer_config.fee_collector_address = fee_collector;
                        // Exactly one batcher; every other validator is sequencing-only.
                        config.batcher_config.enabled = index == 0;
                        config.consensus_config.enabled = true;
                        config.consensus_config.network_key = Some(network_key.into());
                        config.consensus_config.bls_key = Some(bls_key.into());
                        config.consensus_config.listen_address = listen_address;
                        config.consensus_config.validators = committee;
                        // Everything runs on localhost.
                        config.consensus_config.allow_private_ips = true;
                        overrides(index, &mut config);
                        Tester::launch_with_new_runtime(l1, chain_layout, config)
                            .await
                            .with_context(|| format!("failed to launch validator {index}"))
                    }
                });
        let nodes = try_join_all(launches).await?;
        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
            committee_entries: committee,
            bls_keys: keys
                .iter()
                .map(|keys| alloy::hex::encode(keys.bls.encode()))
                .collect(),
        })
    }

    /// One validator's committee entry string, as configured across the cluster.
    /// For an observer this is the entry a *future* committee would list — the
    /// promotion material.
    pub fn committee_entry(&self, index: usize) -> &str {
        &self.committee_entries[index]
    }

    /// One node's BLS signing key (hex). For an observer: generated but never
    /// configured — promotion tests hand it to the node at the role flip.
    pub fn bls_key_hex(&self, index: usize) -> &str {
        &self.bls_keys[index]
    }

    /// The running node at `index`. Panics if that validator is currently stopped —
    /// tests interact only with validators they know to be up.
    pub fn node(&self, index: usize) -> &Tester {
        match &self.validators[index] {
            Validator::Running(node) => node,
            _ => panic!("validator {index} is not running"),
        }
    }

    /// Mutable access to a running validator — for observers that consume node
    /// state, e.g. waiting on the fatal-error handle of a node expected to die.
    pub fn node_mut(&mut self, index: usize) -> &mut Tester {
        match &mut self.validators[index] {
            Validator::Running(node) => node,
            _ => panic!("validator {index} is not running"),
        }
    }

    pub(crate) fn running(&self) -> impl Iterator<Item = (usize, &Tester)> {
        self.validators
            .iter()
            .enumerate()
            .filter_map(|(index, validator)| match validator {
                Validator::Running(node) => Some((index, node)),
                _ => None,
            })
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// The stopped validator at `index` — how offline tooling (the disaster-fork
    /// truncation) reaches its configuration and storage between stop and start.
    pub fn stopped(&self, index: usize) -> &crate::StoppedTester {
        match &self.validators[index] {
            Validator::Stopped(stopped) => stopped,
            _ => panic!("validator {index} is not stopped"),
        }
    }

    /// Gracefully stops one validator; its state, keys, and port reservation stay
    /// around for [`Self::start_validator`].
    pub async fn stop_validator(&mut self, index: usize) -> anyhow::Result<()> {
        let validator = std::mem::replace(&mut self.validators[index], Validator::Transitioning);
        let Validator::Running(node) = validator else {
            anyhow::bail!("validator {index} is not running");
        };
        self.validators[index] = Validator::Stopped(node.stop().await?);
        Ok(())
    }

    /// Restarts a stopped validator on its original state and keys. It rejoins the
    /// committee, backfills what it missed, and participates again.
    pub async fn start_validator(&mut self, index: usize) -> anyhow::Result<()> {
        self.start_validator_with_config_overrides(index, |_| {})
            .await
    }

    /// Like [`Self::start_validator`], but the new incarnation runs a modified
    /// configuration — how tests pin what a misconfigured (or half-upgraded)
    /// validator does to the committee, e.g. one on a different protocol version.
    pub async fn start_validator_with_config_overrides(
        &mut self,
        index: usize,
        configure: impl FnOnce(&mut zksync_os_server::config::Config),
    ) -> anyhow::Result<()> {
        let validator = std::mem::replace(&mut self.validators[index], Validator::Transitioning);
        let Validator::Stopped(stopped) = validator else {
            anyhow::bail!("validator {index} is not stopped");
        };
        // The previous instance's consensus thread winds down asynchronously after the
        // node runtime stops, holding its p2p listener and database handles until the
        // very end. Its storage lock is released last, so "the lock is acquirable"
        // means everything else is gone too — gate the relaunch on it, then on the
        // listen port being bindable. (The lockfile reservation keeps other tests
        // away from the port meanwhile.)
        // Sized for a loaded machine: with the stop-then-release contract, the
        // lock is held until every consensus task has actually wound down.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let instance_lock = zksync_os_server::consensus::instance_lock_path(
            &stopped
                .config()
                .general_config
                .rocks_db_path
                .join("consensus"),
        );
        loop {
            // A consensus-wipe rebuild (`rm -rf <rocks>/consensus`) removes the
            // lock's parent directory too — trivially nobody holds the storage,
            // but the probe needs the directory to exist to create its file (the
            // node's own startup does the same `create_dir_all` before locking).
            if let Some(parent) = instance_lock.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(probe) = std::fs::File::create(&instance_lock)
                && fs2::FileExt::try_lock_exclusive(&probe).is_ok()
            {
                drop(probe);
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the previous consensus instance did not release its storage in time",
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let port = self.consensus_ports[index].port;
        loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(probe) => {
                    drop(probe);
                    break;
                }
                Err(error) => {
                    anyhow::ensure!(
                        tokio::time::Instant::now() < deadline,
                        "consensus port {port} still bound long after shutdown: {error}",
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        // A start the node itself refuses (a startup guard panicking) consumes
        // the instance; restore the slot from the backup so expected-refusal
        // choreography — assert the error, adjust config, start again — can
        // continue against the same stopped state.
        let backup = stopped.backup();
        match stopped.start_with_overrides(configure).await {
            Ok(node) => {
                self.validators[index] = Validator::Running(node);
                Ok(())
            }
            Err(error) => {
                self.validators[index] = Validator::Stopped(backup.restore().await?);
                Err(error)
            }
        }
    }

    /// The highest block height any running validator currently reports.
    pub async fn max_height(&self) -> anyhow::Result<u64> {
        use alloy::providers::Provider as _;
        let mut max = 0;
        for (_, node) in self.running() {
            max = max.max(node.l2_provider.get_block_number().await?);
        }
        Ok(max)
    }

    /// Waits until every *running* validator's RPC reports at least `height`.
    pub async fn wait_for_block_on_all(
        &self,
        height: u64,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        use alloy::providers::Provider as _;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut heights = Vec::with_capacity(self.validators.len());
            let mut all_reached = true;
            for validator in &self.validators {
                match validator {
                    Validator::Running(node) => {
                        let number = node.l2_provider.get_block_number().await.unwrap_or(0);
                        all_reached &= number >= height;
                        heights.push(number.to_string());
                    }
                    _ => heights.push("stopped".to_string()),
                }
            }
            if all_reached {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "running validators did not all reach block {height} within {timeout:?} \
                 (per-validator heights: {heights:?})",
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Asserts every *running* validator serves the identical block hash at `height` —
    /// the RPC-visible form of "all validators committed the same chain".
    pub async fn assert_block_hashes_agree(&self, height: u64) -> anyhow::Result<()> {
        use alloy::eips::BlockId;
        use alloy::providers::Provider as _;
        let mut reference = None;
        for (index, node) in self.running() {
            let block = node
                .l2_provider
                .get_block(BlockId::number(height))
                .await?
                .with_context(|| format!("validator {index} is missing block {height}"))?;
            let hash = block.header.hash;
            match &reference {
                None => reference = Some(hash),
                Some(expected) => anyhow::ensure!(
                    *expected == hash,
                    "validator {index} serves a different block at height {height}: \
                     {hash} != {expected}",
                ),
            }
        }
        Ok(())
    }

    /// Shuts all validators down (concurrently, since a sequential shutdown would make
    /// the remaining quorum-less validators hang on in-flight work).
    pub async fn shutdown_all(self) -> anyhow::Result<()> {
        try_join_all(self.validators.into_iter().map(|validator| async move {
            match validator {
                Validator::Running(node) => node.shutdown().await,
                Validator::Stopped(stopped) => stopped.shutdown().await,
                Validator::Transitioning => Ok(()),
            }
        }))
        .await?;
        Ok(())
    }
}
