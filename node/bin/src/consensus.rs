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
use commonware_runtime::{Metrics as _, Quota, Runner as _, Spawner as _};
use commonware_utils::TryCollect as _;
use commonware_utils::ordered::{BiMap, Map};
use commonware_utils::union_unique;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use zksync_os_consensus_core::types::{Activity, Attributable as _, ConsensusActivity, Scheme};
use zksync_os_consensus_core::{Channels, StackConfig, start_validator};
use zksync_os_consensus_execution::NodeExecutionEnv;
use zksync_os_consensus_execution::metrics::CONSENSUS_METRICS;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_status_server::{ConsensusMetricsEncoder, FinalizedObservation};
use zksync_os_storage_api::{ReadStateHistory, WriteState};
use zksync_os_types::L2Envelope;

use crate::config::ConsensusConfig;

/// Domain-separation namespace for everything this network signs and speaks.
const NAMESPACE: &[u8] = b"zksync-os-consensus";

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

fn decode_hex_key<T: commonware_codec::DecodeExt<()>>(hex: &str) -> anyhow::Result<T> {
    let bytes = alloy::hex::decode(hex.trim()).context("invalid hex")?;
    T::decode(bytes.as_slice()).map_err(|err| anyhow::anyhow!("invalid key encoding: {err}"))
}

/// Everything the consensus thread needs, resolved and validated on the node side
/// before the thread spawns (so misconfiguration fails startup, not a background
/// thread).
pub struct ConsensusSetup {
    pub committee: Vec<CommitteeMember>,
    pub network_key: ed25519::PrivateKey,
    pub scheme: Scheme,
    pub listen_address: SocketAddr,
    pub allow_private_ips: bool,
    pub max_message_size: usize,
    pub storage_directory: PathBuf,
}

impl ConsensusSetup {
    /// Resolves keys and the committee from configuration.
    pub fn from_config(
        config: &ConsensusConfig,
        storage_directory: PathBuf,
    ) -> anyhow::Result<Self> {
        let committee: Vec<CommitteeMember> = config
            .validators
            .iter()
            .map(|entry| {
                parse_committee_member(entry)
                    .with_context(|| format!("bad `consensus.validators` entry: {entry}"))
            })
            .collect::<anyhow::Result<_>>()?;

        let network_key = decode_hex_key::<ed25519::PrivateKey>(
            config
                .network_key
                .as_ref()
                .context("`consensus.network_key` is required")?,
        )
        .context("invalid `consensus.network_key`")?;
        let bls_key = decode_hex_key::<group::Private>(
            config
                .bls_key
                .as_ref()
                .context("`consensus.bls_key` is required")?,
        )
        .context("invalid `consensus.bls_key`")?;

        // The scheme is the committee: the ordered (network identity → consensus key)
        // map every validator must agree on, plus this validator's own signing key.
        let participants: BiMap<ed25519::PublicKey, <MinPk as Variant>::Public> = committee
            .iter()
            .map(|member| (member.network_key.clone(), member.bls_key))
            .try_collect()
            .map_err(|err| anyhow::anyhow!("duplicate committee member: {err:?}"))?;
        let namespace = union_unique(NAMESPACE, b"_CONSENSUS");
        let scheme = Scheme::signer(&namespace, participants, bls_key).context(
            "this validator's BLS key does not belong to any configured committee member",
        )?;

        use commonware_cryptography::Signer as _;
        anyhow::ensure!(
            committee
                .iter()
                .any(|member| member.network_key == network_key.public_key()),
            "this validator's network key does not appear in `consensus.validators`",
        );

        Ok(Self {
            committee,
            network_key,
            scheme,
            listen_address: config
                .listen_address
                .parse()
                .context("invalid `consensus.listen_address`")?,
            allow_private_ips: config.allow_private_ips,
            max_message_size: config.max_message_size,
            storage_directory,
        })
    }
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
        let _ = context_anchor_sender.send(context.clone());
        // From here on the consensus runtime's own registry (engine, marshal, p2p) is
        // live; hand the node a way to scrape it.
        let _ = metrics_encoder_in_runtime.send(Some(std::sync::Arc::new({
            let context = context.clone();
            move || context.encode()
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
            namespace: union_unique(NAMESPACE, b"_P2P"),
            crypto: setup.network_key.clone(),
            listen: setup.listen_address,
            max_message_size: setup.max_message_size as u32,
            mailbox_size: BACKLOG,
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
        let (mut network, mut oracle) = lookup::Network::new(context.with_label("p2p"), p2p_config);

        // The static committee is peer set 0; validator-set changes later mean
        // tracking new sets under new indices.
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
        let peers: commonware_p2p::AddressableTrackedPeers<ed25519::PublicKey> = peers.into();
        oracle.track(0, peers).await;

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
        );

        use commonware_cryptography::Signer as _;
        let identity = setup.network_key.public_key();
        let stack = start_validator(
            context.with_label("validator"),
            StackConfig::new("consensus"),
            identity,
            setup.scheme.clone(),
            env,
            oracle.clone(),
            oracle,
            channels,
            (),
            ActivityObserver {
                finalized: std::sync::Arc::new(observability.finalized),
            },
        )
        .await;

        // Any component exiting is fatal: these tasks run for the life of the node.
        // The shutdown arm (fired explicitly or by the node runtime dropping the
        // sender) is the one non-fatal exit: signal every consensus task to stop and
        // wait for them to wind down, which releases the p2p listener and journals.
        tokio::select! {
            _ = shutdown => {
                tracing::info!("node is shutting down; stopping consensus");
                context
                    .clone()
                    .stop(0, Some(std::time::Duration::from_secs(10)))
                    .await
                    .context("consensus tasks did not stop in time")?;
                Ok(())
            }
            _ = network_handle => anyhow::bail!("consensus networking exited unexpectedly"),
            _ = stack.engine => anyhow::bail!("consensus engine exited unexpectedly"),
            _ = stack.marshal => anyhow::bail!("consensus marshal exited unexpectedly"),
            _ = stack.broadcast => anyhow::bail!("consensus broadcast exited unexpectedly"),
        }
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
    mut sender: TxSender,
    mut receiver: TxReceiver,
    max_message_size: usize,
) where
    C: commonware_runtime::Spawner + commonware_runtime::Metrics,
    P: L2Subpool + Clone,
    TxSender: Sender<PublicKey = ed25519::PublicKey>,
    TxReceiver: Receiver<PublicKey = ed25519::PublicKey>,
{
    // Leave generous headroom under the network's message cap; a batch is cut early
    // when it grows past this.
    let byte_budget = max_message_size / 2;

    let gossip_pool = pool.clone();
    context
        .with_label("tx_gossip_out")
        .spawn(move |task_context| async move {
            // The pool's listener never closes on consensus shutdown (the pool lives
            // node-side), so this task must watch the stop signal itself — a parked
            // task would hold pool handles (and the databases under them) past the
            // runtime's shutdown deadline.
            let mut stopped = task_context.stopped();
            let mut new_txs = gossip_pool.new_transactions_listener();
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
                if sender
                    .send(commonware_p2p::Recipients::All, message, false)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

    context
        .with_label("tx_gossip_in")
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
}

/// Feeds consensus activity into metrics and the status tip. Fault evidence — proof a
/// committee member signed contradicting votes — is the loudest signal a validator
/// can produce: it must stay absent on a healthy committee.
#[derive(Clone)]
struct ActivityObserver {
    finalized: std::sync::Arc<tokio::sync::watch::Sender<Option<FinalizedObservation>>>,
}

impl zksync_os_consensus_core::types::Reporter for ActivityObserver {
    type Activity = ConsensusActivity;

    async fn report(&mut self, activity: Self::Activity) {
        let kind = match &activity {
            Activity::Notarize(_) => "notarize",
            Activity::Notarization(_) => "notarization",
            Activity::Certification(_) => "certification",
            Activity::Nullify(_) => "nullify",
            Activity::Nullification(_) => "nullification",
            Activity::Finalize(_) => "finalize",
            Activity::Finalization(finalization) => {
                let round = finalization.round();
                let _ = self.finalized.send(Some(FinalizedObservation {
                    epoch: round.epoch().get(),
                    view: round.view().get(),
                    observed_unix: unix_now(),
                }));
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
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
