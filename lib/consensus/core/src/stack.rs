//! Assembles one validator's full consensus stack.
//!
//! A running validator is a small constellation of cooperating components:
//!
//! ```text
//!                 votes / certificates / certificate-backfill        (3 p2p channels)
//!                        ▲                    ▲
//!                        ▼                    ▼
//!                  simplex engine  ── reports activity ──►  marshal ◄── block-backfill
//!                        │                                    ▲  │       (p2p channel)
//!             Automaton/Relay calls                           │  │ ordered, acked
//!                        ▼                                    │  ▼ finalized blocks
//!                  Inline application  ── verified blocks ────┘  FinalizedBlockCommitter
//!                        │                                              │
//!                        ▼                                              ▼
//!                  ExecutionApplication ──────────────────────►  ExecutionEnv
//!                        ▲
//!                        │ full blocks (gossip)
//!                  buffered broadcast  ◄── block-broadcast p2p channel
//! ```
//!
//! - The **simplex engine** runs the consensus protocol itself over digests: proposals,
//!   votes, notarizations, finalizations, view timeouts. It journals every vote to disk
//!   before broadcasting it, which is what makes an unclean restart unable to double-sign.
//! - **marshal** turns consensus outcomes into an ordered stream of finalized blocks:
//!   it caches blocks arriving via gossip, verifies finality certificates, backfills
//!   anything missing from peers, archives blocks + certificates durably, and delivers
//!   each finalized block exactly-in-order (at-least-once) to the committer.
//! - The **Inline application** (from marshal) bridges the engine's digest-world to the
//!   block-world: it resolves digests to blocks, enforces parent/height structure and
//!   epoch-boundary rules, and only then asks our application to judge content validity —
//!   so a validator fully verifies a block before ever voting for it.
//! - [`ExecutionApplication`] + [`FinalizedBlockCommitter`] adapt all of the above to the
//!   node's [`ExecutionEnv`].
//!
//! One stack instance = one validator. Within it, **one simplex engine exists per
//! epoch**: as the chain crosses an epoch boundary, the stack starts the next epoch's
//! engine (journaling under its own partition) and retires the previous one once its
//! tail is finalized — while marshal, the broadcast layer, and the application keep
//! running across boundaries. The engine channels are multiplexed by epoch id, so two
//! engines can coexist during the handoff without seeing each other's traffic. The
//! handoff itself is protocol-level and upstream: the new epoch's first proposal
//! re-proposes the previous epoch's boundary block, so the new committee begins by
//! re-certifying where the old one stopped.

use crate::application::ExecutionApplication;
use crate::committer::FinalizedBlockCommitter;
use crate::execution::ExecutionEnv;
use crate::storage::{init_blocks_archive, init_finalizations_archive};
use crate::types::{Elector, Scheme, SchemeProvider};
use commonware_actor::Feedback;
use commonware_broadcast::buffered;
use commonware_consensus::marshal::standard::{Inline, Standard};
use commonware_consensus::marshal::{self, core as marshal_core, resolver};
use commonware_consensus::simplex::config::{Floor, ForwardingPolicy};
use commonware_consensus::simplex::types::{Activity, Certificate};
use commonware_consensus::simplex::{Engine, config::Config as EngineConfig};
use commonware_consensus::types::{Epoch, FixedEpocher, Height, ViewDelta};
use commonware_consensus::{Reporter, Reporters};
use commonware_cryptography::Digestible;
use commonware_cryptography::certificate::Scheme as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::utils::mux::{MuxHandle, Muxer, SubReceiver, SubSender};
use commonware_parallel::Sequential;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_runtime::{BufferPooler, Clock, Handle, Metrics, Spawner, Storage};
use commonware_storage::archive::{Archive as _, Identifier};
use commonware_utils::{NZU16, NZUsize};
use rand08::{CryptoRng, Rng};
use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

/// Tuning knobs for one validator stack. `new` gives values suitable for tests and local
/// networks; production profiles adjust the timeouts to real network latencies.
#[derive(Clone)]
pub struct StackConfig {
    /// Namespace for everything this stack persists (journals, archives). Must be unique
    /// per validator on shared storage, and must stay stable across restarts — this is
    /// how a restarted validator finds its own vote journal and refuses to double-sign.
    pub partition_prefix: String,
    /// Number of blocks per epoch. The validator set is fixed within an epoch.
    pub epoch_length: NonZeroU64,
    /// How long to wait for the leader's proposal before voting to skip the view.
    pub leader_timeout: Duration,
    /// How long to wait for notarization progress before voting to skip the view.
    pub certification_timeout: Duration,
    /// How often to re-broadcast the skip vote while a view is stuck.
    pub timeout_retry: Duration,
    /// Timeout for a single backfill request to a peer.
    pub fetch_timeout: Duration,
    /// How many views below the finalized tip stay tracked (and journaled).
    pub activity_timeout: ViewDelta,
    /// Skip the leader wait entirely if the elected leader was silent for this many views.
    pub skip_timeout: ViewDelta,
    /// Bound on internal actor mailboxes (backpressure).
    pub mailbox_size: NonZeroUsize,
    /// How many recently-gossiped blocks the broadcast cache keeps per peer.
    pub deque_size: usize,
    /// How many finalized blocks may be in flight to the committer before marshal stops
    /// delivering more (the execution side is the pacer).
    pub max_pending_acks: NonZeroUsize,
    /// How long marshal retains per-view scratch data after processing.
    pub view_retention_timeout: ViewDelta,
    /// Cap on concurrent backfill repairs.
    pub max_repair: NonZeroUsize,
    /// How many *retired* epochs of consensus storage to keep; anything older is
    /// pruned (vote-journal partitions removed, marshal's finalized archives
    /// pruned below the horizon). `None` keeps everything. A per-node setting —
    /// pruning local storage needs no committee coordination — but with every
    /// peer pruning, history below everyone's horizon is gone from the network:
    /// a rebuild then starts from a finality floor, not from the era genesis.
    /// The node's own finality store (the sovereign certificate trail) is never
    /// touched by this.
    pub epoch_retention: Option<NonZeroU64>,
    /// Heights per storage section in marshal's finalized archives — the
    /// granularity pruning works at: only sections entirely below the prune
    /// horizon are dropped. The default suits production epoch lengths (many
    /// sections per epoch); tests with tiny epochs shrink it so pruning is
    /// observable at their scale.
    pub archive_items_per_section: NonZeroU64,
}

impl StackConfig {
    pub fn new(partition_prefix: impl Into<String>) -> Self {
        Self {
            partition_prefix: partition_prefix.into(),
            // Effectively one unbounded epoch by default; deployments choose a real
            // length via configuration. Engine rotation works at any length — tests
            // run epochs of a few blocks.
            epoch_length: NonZeroU64::new(1_000_000_000).expect("nonzero"),
            leader_timeout: Duration::from_secs(1),
            certification_timeout: Duration::from_secs(2),
            timeout_retry: Duration::from_secs(10),
            fetch_timeout: Duration::from_secs(1),
            activity_timeout: ViewDelta::new(10),
            skip_timeout: ViewDelta::new(5),
            mailbox_size: NZUsize!(1024),
            deque_size: 16,
            max_pending_acks: NonZeroUsize::new(4).expect("nonzero"),
            view_retention_timeout: ViewDelta::new(100),
            max_repair: NonZeroUsize::new(16).expect("nonzero"),
            epoch_retention: None,
            archive_items_per_section: NonZeroU64::new(1024).expect("nonzero"),
        }
    }
}

impl StackConfig {
    /// Overrides the epoch length (tests run short epochs to cross boundaries fast).
    pub fn with_epoch_length(mut self, length: NonZeroU64) -> Self {
        self.epoch_length = length;
        self
    }
}

/// The five p2p channels one validator uses, in one place so composition roots (real
/// networking and simulated alike) register them consistently.
///
/// The first three belong to the simplex engine, the last two to block dissemination:
/// consensus messages carry only 32-byte digests, while full blocks travel separately
/// via gossip (`block_broadcast`) with gap-repair over `block_backfill`.
pub struct Channels<TSender, TReceiver> {
    /// Individual consensus votes (notarize / nullify / finalize).
    pub votes: (TSender, TReceiver),
    /// Recovered certificates (notarizations / nullifications / finalizations).
    pub certificates: (TSender, TReceiver),
    /// Request/response backfill of certificates the engine missed.
    pub certificate_backfill: (TSender, TReceiver),
    /// Gossip of full blocks.
    pub block_broadcast: (TSender, TReceiver),
    /// Request/response backfill of finalized blocks and finalizations.
    pub block_backfill: (TSender, TReceiver),
}

/// An extra-reporter that observes nothing. Production stacks use this unless they
/// attach a metrics observer; tests attach recorders instead.
pub struct NullReporter<A>(std::marker::PhantomData<A>);

impl<A> NullReporter<A> {
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<A> Default for NullReporter<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> Clone for NullReporter<A> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<A: Send + 'static> Reporter for NullReporter<A> {
    type Activity = A;

    fn report(&mut self, _activity: Self::Activity) -> Feedback {
        Feedback::Ok
    }
}

/// The engines currently running, one per live epoch — shared between the epoch
/// rotation task (which starts and retires them) and [`ValidatorStack::abort`]
/// (which must kill whatever is alive when the whole validator stops).
pub type EngineRegistry = Arc<Mutex<BTreeMap<u64, Handle<()>>>>;

/// The storage partition one epoch's engine journals its votes under. Stable
/// across restarts (a restarted engine must find its own journal to avoid
/// double-signing) and per-epoch (so retiring an epoch can drop exactly its
/// journal). Public for storage probes (tests, debugging tooling).
pub fn engine_partition(partition_prefix: &str, epoch: u64) -> String {
    format!("{partition_prefix}-engine-epoch-{epoch}")
}

/// What every engine reports to: marshal first (finalizations drive ordered delivery
/// and backfill), then the node's extra reporter (metrics, observers). Named so the
/// `Reporters::from` call site can state its target type — the tuple `From` impls
/// are ambiguous without an annotation.
type EngineReporters<X, TReporter> = Reporters<
    Activity<Scheme, <<X as ExecutionEnv>::Block as Digestible>::Digest>,
    marshal_core::Mailbox<Scheme, Standard<<X as ExecutionEnv>::Block>>,
    TReporter,
>;

/// Where this validator's consensus state begins when its local consensus storage
/// is empty (a fresh validator, a promoted EN, a rebuild after an incident).
///
/// With *existing* archives the choice barely matters: marshal resumes from its own
/// durable state, and a floor at or below what it already processed is ignored with
/// a warning. The variants differ for empty storage:
///
/// - `Genesis`: consensus history is reconstructed from the era genesis — a full
///   block backfill from peers, O(chain). Always correct; the only option for a
///   chain's first start.
/// - `Floor`: consensus starts from a finalized checkpoint — marshal fetches the
///   floor block, delivers from it forward, and **never fetches below it**,
///   bounding catch-up to O(chain tip − floor). The floor's epoch engine starts
///   from the finalization itself instead of an epoch-anchor block.
///
/// Caller contract for `Floor`: the finalization must verify under the schedule,
/// must not be the era genesis (height zero — use `Genesis`), and its block height
/// must be **at or below the environment's committed tip**. A floor above the tip
/// leaves the environment with a delivery gap it can never fill (marshal refuses
/// to fetch below the floor); a floor at the tip is the ideal (delivery resumes
/// with an idempotent re-commit of the tip, then new blocks).
#[derive(Clone)]
pub enum StackStart<D: commonware_cryptography::Digest> {
    Genesis,
    // Boxed for the variant-size lint: a Finalization carries a whole certificate,
    // and StackStart values are built once per start — indirection is free here.
    Floor(Box<commonware_consensus::simplex::types::Finalization<Scheme, D>>),
}

/// Handles to a running validator stack. Aborting all handles stops the validator;
/// starting a fresh stack with the same `partition_prefix` over the same storage
/// resumes it (journal replay restores consensus state without double-signing).
pub struct ValidatorStack<B: commonware_consensus::Block> {
    pub epoch_manager: Handle<()>,
    pub engines: EngineRegistry,
    /// Channel muxes and the tip scout — support tasks that live as long as the
    /// validator.
    pub support_tasks: Vec<Handle<()>>,
    pub marshal: Handle<()>,
    pub broadcast: Handle<()>,
    /// Query surface into marshal (finalized tip, blocks by height/digest).
    pub marshal_mailbox: marshal_core::Mailbox<Scheme, Standard<B>>,
}

impl<B: commonware_consensus::Block> ValidatorStack<B> {
    /// Stops all components. The stack can be started again over the same storage.
    pub fn abort(&self) {
        self.epoch_manager.abort();
        for engine in self.engines.lock().unwrap().values() {
            engine.abort();
        }
        for task in &self.support_tasks {
            task.abort();
        }
        self.marshal.abort();
        self.broadcast.abort();
    }
}

/// Builds and starts every component of one validator: broadcast, backfill, archives,
/// marshal, the application adapter, and one simplex engine per epoch this validator
/// is a committee member of (per `scheme_provider`'s schedule).
#[allow(clippy::too_many_arguments)]
pub async fn start_validator<R, X, TSender, TReceiver, TBlocker, TPeers, TReporter>(
    context: R,
    config: StackConfig,
    identity: PublicKey,
    scheme_provider: SchemeProvider,
    mut env: X,
    blocker: TBlocker,
    peer_provider: TPeers,
    channels: Channels<TSender, TReceiver>,
    block_codec_config: <X::Block as commonware_codec::Read>::Cfg,
    // Observer for raw consensus activity (votes, certificates, fault evidence) beyond
    // what marshal consumes — metrics in production, assertion recorders in tests.
    extra_reporter: TReporter,
    start: StackStart<<X::Block as Digestible>::Digest>,
) -> ValidatorStack<X::Block>
where
    R: Clock + Spawner + Metrics + Storage + BufferPooler + Rng + CryptoRng,
    X: ExecutionEnv,
    <X::Block as commonware_codec::Read>::Cfg: Clone + Send + Sync + 'static,
    TSender: commonware_p2p::Sender<PublicKey = PublicKey>,
    TReceiver: commonware_p2p::Receiver<PublicKey = PublicKey>,
    TBlocker: commonware_p2p::Blocker<PublicKey = PublicKey> + Clone,
    TPeers: commonware_p2p::Provider<PublicKey = PublicKey> + Clone,
    TReporter: Reporter<Activity = Activity<Scheme, <X::Block as Digestible>::Digest>>,
{
    let page_cache = CacheRef::from_pooler(&context, NZU16!(1024), NZUsize!(128));

    // Block gossip: every proposed block is pushed to all peers and cached, so followers
    // can resolve a digest to the full block without asking the leader.
    let (broadcast_engine, broadcast_mailbox) = buffered::Engine::new(
        context.child("block_broadcast"),
        buffered::Config {
            public_key: identity.clone(),
            mailbox_size: config.mailbox_size,
            deque_size: config.deque_size,
            priority: false,
            codec_config: block_codec_config.clone(),
            peer_provider: peer_provider.clone(),
        },
    );
    let broadcast = broadcast_engine.start(channels.block_broadcast);

    // Block backfill: how a node that missed gossip (offline, partitioned, late-joining)
    // pulls finalized blocks and certificates from its peers.
    let block_resolver = resolver::p2p::init(
        context.child("block_backfill"),
        resolver::p2p::Config {
            public_key: identity.clone(),
            peer_provider: peer_provider.clone(),
            blocker: blocker.clone(),
            mailbox_size: config.mailbox_size,
            initial: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            fetch_retry_timeout: Duration::from_millis(100),
            priority_requests: false,
            priority_responses: false,
        },
        channels.block_backfill,
    );

    let finalizations = init_finalizations_archive(
        &context,
        &config.partition_prefix,
        page_cache.clone(),
        config.archive_items_per_section,
    )
    .await;
    let blocks = init_blocks_archive::<_, X::Block>(
        &context,
        &config.partition_prefix,
        page_cache.clone(),
        block_codec_config.clone(),
        config.archive_items_per_section,
    )
    .await;

    // The execution side persists chain content but not consensus identities; after a
    // restart it knows how tall its chain is, not which digest its tip has. The block
    // archive holds the finalized block at that height — hand it back before anything
    // builds on the tip.
    if let Some(committed) = env.committed_height().await
        && let Ok(Some(block)) = blocks.get(Identifier::Index(committed.get())).await
    {
        env.adopt_committed_block(&block).await;
    }

    let epocher = FixedEpocher::new(config.epoch_length);
    // Where marshal's chain begins. `Genesis`: the era anchor is consensus height
    // zero — the environment's genesis block stands for the chain's anchor (its own
    // genesis, or the migration cutover block), and every consensus-side height
    // counts from it. `Floor`: a finalized checkpoint — marshal verifies it, fetches
    // its block from peers, and syncs strictly above it (see [`StackStart`]).
    let marshal_start = match &start {
        StackStart::Genesis => marshal::Start::Genesis(env.genesis_block().await),
        StackStart::Floor(finalization) => marshal::Start::Floor((**finalization).clone()),
    };
    let (marshal_actor, marshal_mailbox, _marshal_height) = marshal_core::Actor::init(
        context.child("marshal"),
        finalizations,
        blocks,
        marshal::Config {
            // Marshal verifies certificates from arbitrary historical epochs during
            // backfill — the provider resolves each epoch to its committee's scheme.
            provider: scheme_provider.clone(),
            epocher: epocher.clone(),
            start: marshal_start,
            partition_prefix: config.partition_prefix.clone(),
            mailbox_size: config.mailbox_size,
            view_retention_timeout: config.view_retention_timeout,
            prunable_items_per_section: config.archive_items_per_section,
            page_cache: page_cache.clone(),
            replay_buffer: NZUsize!(1024 * 1024),
            key_write_buffer: NZUsize!(1024 * 1024),
            value_write_buffer: NZUsize!(1024 * 1024),
            max_repair: config.max_repair,
            max_pending_acks: config.max_pending_acks,
            block_codec_config: block_codec_config.clone(),
            strategy: Sequential,
        },
    )
    .await;
    // (A node whose archives trail its chain re-delivers old blocks on startup;
    // `commit` absorbs the replays as no-ops. The explicit floor-skip that existed
    // before the upgrade required a finalization we may not hold — correctness never
    // depended on it.)

    let (committer, commit_worker) =
        FinalizedBlockCommitter::spawn(context.child("committer"), env.clone());
    let marshal = marshal_actor.start(committer, broadcast_mailbox, block_resolver);

    // The application half: judge and build blocks. `Inline` = full verification happens
    // before this validator votes, never after.
    let application = Inline::new(
        context.child("application"),
        ExecutionApplication::new(env.clone()),
        marshal_mailbox.clone(),
        epocher.clone(),
    );

    // The engine channels are multiplexed by epoch id: during an epoch handoff two
    // engines are briefly alive at once (the old one finalizing its tail, the new
    // one re-certifying the boundary block), and each must see only its own epoch's
    // traffic. Messages for epochs nobody runs anymore are dropped by the muxer.
    let (votes_muxer, votes_mux) = Muxer::new(
        context.child("votes_mux"),
        channels.votes.0,
        channels.votes.1,
        config.mailbox_size.get(),
    );
    let (certificates_muxer, certificates_mux, certificate_backup) = {
        use commonware_p2p::utils::mux::Builder as _;
        Muxer::builder(
            context.child("certificates_mux"),
            channels.certificates.0,
            channels.certificates.1,
            config.mailbox_size.get(),
        )
        .with_backup()
        .build()
    };
    let (certificate_backfill_muxer, certificate_backfill_mux) = Muxer::new(
        context.child("certificate_backfill_mux"),
        channels.certificate_backfill.0,
        channels.certificate_backfill.1,
        config.mailbox_size.get(),
    );
    let mut support_tasks = vec![
        // A mux only exits when its underlying channel dies, which means the p2p
        // network died — the node watches the network task itself, so these only
        // need to leave a trace.
        context.child("votes_mux_task").spawn(|_| async move {
            if let Err(err) = votes_muxer.run().await {
                tracing::error!(?err, "votes mux exited");
            }
        }),
        context
            .child("certificates_mux_task")
            .spawn(|_| async move {
                if let Err(err) = certificates_muxer.run().await {
                    tracing::error!(?err, "certificates mux exited");
                }
            }),
        context
            .child("certificate_backfill_mux_task")
            .spawn(|_| async move {
                if let Err(err) = certificate_backfill_muxer.run().await {
                    tracing::error!(?err, "certificate backfill mux exited");
                }
            }),
    ];

    // Certificates for epochs this validator runs flow to their engines through the
    // mux; certificates for epochs it does not run land on the backup lane. One of
    // them is exactly how a validator that slept through epoch boundaries discovers
    // the chain moved on: a *valid* finalization from a later epoch is self-proving
    // evidence of the real tip, no matter which peer sent it or what its message was
    // tagged with. Verify it, hand it to marshal — marshal backfills the gap, the
    // committed height advances, and the epoch rotation follows it forward. Without
    // this, a validator more than one epoch behind would wait forever: its old
    // engine hears nothing (peers retired that epoch), and the current epoch's
    // traffic would be dropped unheard.
    let tip_scout = context.child("tip_scout").spawn({
        let scheme_provider = scheme_provider.clone();
        let mut marshal_mailbox = marshal_mailbox.clone();
        let mut scout_reporter = extra_reporter.clone();
        let mut certificate_backup = certificate_backup;
        move |mut scout_context| async move {
            // The epoch — and with it the committee whose signatures to check — is
            // *inside* the certificate, so decoding must precede scheme selection:
            // decode with the unbounded config, then verify against the scheme of
            // the epoch the certificate claims. A forged epoch claim just fails
            // verification against that epoch's real committee.
            let codec_config = Scheme::certificate_codec_config_unbounded();
            while let Some((_tag, (_peer, bytes))) = certificate_backup.recv().await {
                use commonware_codec::Decode as _;
                let bytes: &[u8] = bytes.as_ref();
                let Ok(certificate) =
                    Certificate::<Scheme, <X::Block as Digestible>::Digest>::decode_cfg(
                        bytes,
                        &codec_config,
                    )
                else {
                    continue;
                };
                let Certificate::Finalization(finalization) = certificate else {
                    continue;
                };
                let scheme = scheme_provider.scheme_for(finalization.round().epoch());
                if !finalization.verify(&mut scout_context, scheme.as_ref(), &Sequential) {
                    continue;
                }
                info!(
                    round = ?finalization.round(),
                    "verified a finalization from an epoch this validator is not running; \
                     handing it to marshal for catch-up"
                );
                // The observer hears it too: for a validator following epochs it is
                // not a member of (scheduled out, or catching up from far behind),
                // scout-verified finalizations are its only view of finality — they
                // keep `/status` truthful and the sovereign certificate/custody
                // trail complete.
                let _ = scout_reporter.report(Activity::Finalization(finalization.clone()));
                let _ = marshal_mailbox.report(Activity::Finalization(finalization));
            }
        }
    });

    // Everything one engine needs, captured once; the rotation task calls this for
    // each epoch it starts — and only for epochs where this validator is a committee
    // member (the rotation task checks membership before calling). The closure keeps
    // `start_validator` the single place where an engine's configuration is spelled
    // out.
    let spawn_engine = {
        // Engines are children of one supervision node; the epoch rides as a metric
        // attribute (dynamic values stay out of metric names).
        let context = context.child("engines");
        let config = config.clone();
        let scheme_provider = scheme_provider.clone();
        let marshal_mailbox = marshal_mailbox.clone();
        move |epoch: Epoch,
              floor: Floor<Scheme, <X::Block as Digestible>::Digest>,
              votes: (SubSender<TSender>, SubReceiver<TReceiver>),
              certificates: (SubSender<TSender>, SubReceiver<TReceiver>),
              certificate_backfill: (SubSender<TSender>, SubReceiver<TReceiver>)|
              -> Handle<()> {
            let reporter: EngineReporters<X, TReporter> =
                Reporters::from((marshal_mailbox.clone(), extra_reporter.clone()));
            let engine = Engine::new(
                context
                    .child("engine")
                    .with_attribute("epoch", epoch.get().to_string()),
                EngineConfig {
                    scheme: scheme_provider.scheme_for(epoch).as_ref().clone(),
                    elector: Elector::default(),
                    blocker: blocker.clone(),
                    automaton: application.clone(),
                    relay: application.clone(),
                    // Marshal consumes consensus outcomes (certificates drive
                    // finalization); the extra reporter observes alongside it.
                    reporter,
                    strategy: Sequential,
                    partition: engine_partition(&config.partition_prefix, epoch.get()),
                    mailbox_size: config.mailbox_size,
                    epoch,
                    // Where this engine's chain of certificates begins — the epoch's
                    // anchor block, or a floor finalization inside the epoch (the
                    // rotation task decides; see `run_epoch_rotation`).
                    floor,
                    replay_buffer: NZUsize!(1024 * 1024),
                    write_buffer: NZUsize!(1024 * 1024),
                    page_cache: page_cache.clone(),
                    leader_timeout: config.leader_timeout,
                    certification_timeout: config.certification_timeout,
                    timeout_retry: config.timeout_retry,
                    activity_timeout: config.activity_timeout,
                    skip_timeout: config.skip_timeout,
                    fetch_timeout: config.fetch_timeout,
                    fetch_concurrent: NZUsize!(4),
                    forwarding: ForwardingPolicy::Disabled,
                },
            );
            engine.start(votes, certificates, certificate_backfill)
        }
    };

    let engines: EngineRegistry = Arc::new(Mutex::new(BTreeMap::new()));
    let rotation_marshal = marshal_mailbox.clone();
    // The floor rides into rotation: the floor's own epoch has no locally-known
    // anchor block (marshal never fetches below the floor), so its engine starts
    // from the floor finalization instead.
    let stack_floor = match start {
        StackStart::Genesis => None,
        StackStart::Floor(finalization) => Some(*finalization),
    };
    let epoch_manager = context.child("epoch_manager").spawn({
        let engines = engines.clone();
        let partition_prefix = config.partition_prefix.clone();
        let epoch_retention = config.epoch_retention;
        move |context| {
            run_epoch_rotation(
                context,
                epocher,
                scheme_provider,
                env,
                rotation_marshal,
                stack_floor,
                partition_prefix,
                epoch_retention,
                votes_mux,
                certificates_mux,
                certificate_backfill_mux,
                engines,
                spawn_engine,
            )
        }
    });

    support_tasks.push(tip_scout);
    support_tasks.push(commit_worker);

    ValidatorStack {
        epoch_manager,
        engines,
        support_tasks,
        marshal,
        broadcast,
        marshal_mailbox,
    }
}

/// How often the rotation task re-derives which epochs should be running. Boundary
/// crossings are rare (epochs are hours in production), so a cheap periodic check
/// beats wiring the task into the finalization stream; the interval only bounds how
/// long after a boundary the next engine appears, and one extra view timeout at a
/// boundary is routine.
const EPOCH_POLL: Duration = Duration::from_millis(250);

/// Keeps exactly the right engines alive as the chain crosses epoch boundaries.
///
/// The invariant: the engine for the epoch of the *next block to decide* is always
/// running, and the engine for the epoch of the *committed tip* stays alive until the
/// tip moves past its epoch (during a handoff those are different epochs — the old
/// engine is still finalizing its tail while the new one re-certifies the boundary
/// block). Everything older is retired: its journal stays on disk, so nothing about
/// double-sign protection changes if the same epoch were ever revisited on restart.
///
/// Engines exist only for epochs where this validator is in the committee. A
/// validator scheduled out of epoch E simply never builds E's engine: it keeps
/// marshal, broadcast, and the tip scout (so it can still observe and serve
/// history), but it takes no further part in deciding blocks — the operational
/// path for a machine that should keep following the chain is to repoint it as an
/// external node.
#[allow(clippy::too_many_arguments)]
async fn run_epoch_rotation<R, X, TSender, TReceiver, F>(
    context: R,
    epocher: FixedEpocher,
    scheme_provider: SchemeProvider,
    mut env: X,
    marshal: marshal_core::Mailbox<Scheme, Standard<X::Block>>,
    stack_floor: Option<
        commonware_consensus::simplex::types::Finalization<
            Scheme,
            <X::Block as Digestible>::Digest,
        >,
    >,
    partition_prefix: String,
    epoch_retention: Option<NonZeroU64>,
    mut votes_mux: MuxHandle<TSender, TReceiver>,
    mut certificates_mux: MuxHandle<TSender, TReceiver>,
    mut certificate_backfill_mux: MuxHandle<TSender, TReceiver>,
    engines: EngineRegistry,
    spawn_engine: F,
) where
    R: Clock + Spawner + Metrics + Storage,
    X: ExecutionEnv,
    TSender: commonware_p2p::Sender<PublicKey = PublicKey>,
    TReceiver: commonware_p2p::Receiver<PublicKey = PublicKey>,
    F: Fn(
        Epoch,
        Floor<Scheme, <X::Block as Digestible>::Digest>,
        (SubSender<TSender>, SubReceiver<TReceiver>),
        (SubSender<TSender>, SubReceiver<TReceiver>),
        (SubSender<TSender>, SubReceiver<TReceiver>),
    ) -> Handle<()>,
{
    use commonware_consensus::types::Epocher as _;
    use futures::FutureExt as _;
    // Epochs we've already logged "not a member" for — the poll re-derives the same
    // answer every tick, the operator needs to hear it once. Pruned with retirement.
    let mut announced_outside = std::collections::BTreeSet::new();
    // Epochs whose anchor-wait was already announced (same once-per-epoch rule).
    let mut announced_waiting = std::collections::BTreeSet::new();
    // Epochs whose registry-derivation wait was already announced (same rule).
    let mut announced_unsettled = std::collections::BTreeSet::new();
    // Epochs below this have had their storage pruned (this run; restarts
    // re-derive it by re-walking, which is idempotent).
    let mut pruned_below: u64 = 0;
    let mut stopped = context.stopped();
    loop {
        // A graceful stop makes engines exit on purpose (they watch the same
        // signal), and the signal is set before any of them can react to it — so
        // checking it first each tick keeps the death-check below from reading a
        // planned shutdown as an engine crash. Found the hard way: on the real
        // runtime a node stop could land between an engine's exit and this task's
        // abort, panicking a perfectly healthy shutdown.
        if (&mut stopped).now_or_never().is_some() {
            return;
        }

        // Every engine in the registry is one we *want* running (retirement removes
        // handles before aborting them). A resolved handle here therefore means an
        // engine died on its own — make that as loud as the pre-rotation stack did,
        // by taking the whole rotation task (watched by the node) down with it.
        {
            let mut engines = engines.lock().unwrap();
            for (epoch, handle) in engines.iter_mut() {
                if handle.now_or_never().is_some() {
                    panic!("consensus engine for epoch {epoch} exited unexpectedly");
                }
            }
        }

        let committed = env
            .committed_height()
            .await
            .map(|height| height.get())
            .unwrap_or(0);

        // The epoch that must decide the next block, and the epoch the committed tip
        // still lives in. Distinct exactly during a handoff.
        let Some(active) = epocher.containing(Height::new(committed + 1)) else {
            context.sleep(EPOCH_POLL).await;
            continue;
        };
        let tail = epocher
            .containing(Height::new(committed.max(1)))
            .unwrap_or(active);

        for epoch in [tail.epoch(), active.epoch()] {
            let key = epoch.get();
            if engines.lock().unwrap().contains_key(&key) {
                continue;
            }
            // A registry-governed epoch whose derivation has not landed yet has
            // no committee to build an engine over — the scheme below would be a
            // stale clamp. Wait; the derivation driver follows the applied
            // height, which trails the committed height this rotation follows,
            // so the gap closes as commits drain (production lookahead is a full
            // epoch, making this gate a startup/catch-up phenomenon only).
            if !scheme_provider.settled_for(epoch) {
                if announced_unsettled.insert(key) {
                    info!(
                        epoch = key,
                        "epoch's committee is not yet derived from the registry; \
                         engine start waits for the derivation"
                    );
                }
                continue;
            }
            announced_unsettled.remove(&key);
            // Committee membership gates the engine: a validator outside this
            // epoch's committee has no key in its scheme and casts no votes.
            if scheme_provider.scheme_for(epoch).me().is_none() {
                if announced_outside.insert(key) {
                    info!(
                        epoch = key,
                        "not in the committee for this epoch; no engine will run"
                    );
                }
                continue;
            }
            // The engine starts from its epoch's anchor block: the last block of
            // the previous epoch (already committed by the time this epoch becomes
            // wanted — the rotation is committed-height-driven), or the era genesis
            // for epoch 0. Marshal holds both.
            let anchor_height = epocher
                .first(epoch)
                .map(|first| Height::new(first.get().saturating_sub(1)))
                .unwrap_or_else(Height::zero);
            let engine_floor = match marshal
                .get_info(marshal::Identifier::Height(anchor_height))
                .await
            {
                Some((_, anchor_digest)) => Floor::Genesis(anchor_digest),
                // A floor-started validator never obtains blocks below its floor,
                // so the floor's own epoch has no anchor block to offer — the
                // engine starts from the floor finalization itself. (Upstream
                // asserts the finalization belongs to the configured epoch, hence
                // the epoch guard.)
                None if stack_floor
                    .as_ref()
                    .is_some_and(|floor| floor.round().epoch() == epoch) =>
                {
                    let floor = stack_floor.clone().expect("checked above");
                    Floor::Finalized(floor)
                }
                None => {
                    // The anchor is not locally available yet. Usually transient
                    // (a fresh validator still backfilling toward the boundary) —
                    // but permanent when every peer has pruned the anchor's
                    // height, which is exactly the state a consensus rebuild over
                    // pruned peers lands in without a floor: following forever,
                    // voting never. Say so once, with the remedy.
                    if announced_waiting.insert(key) {
                        info!(
                            epoch = key,
                            anchor_height = anchor_height.get(),
                            "engine is waiting for its epoch's anchor block; if                              peers no longer serve that height, restart this                              validator from a finality floor"
                        );
                    }
                    continue;
                }
            };
            let votes = votes_mux.register(key).await.expect("register votes mux");
            let certificates = certificates_mux
                .register(key)
                .await
                .expect("register certificates mux");
            let certificate_backfill = certificate_backfill_mux
                .register(key)
                .await
                .expect("register certificate backfill mux");
            announced_waiting.remove(&key);
            info!(epoch = key, "starting consensus engine for epoch");
            let handle = spawn_engine(
                epoch,
                engine_floor,
                votes,
                certificates,
                certificate_backfill,
            );
            engines.lock().unwrap().insert(key, handle);
        }

        // Retire engines whose epoch the committed tip has fully moved past.
        let keep_from = tail.epoch().get();
        engines.lock().unwrap().retain(|&epoch, handle| {
            if epoch < keep_from {
                info!(epoch, "retiring consensus engine for finished epoch");
                handle.abort();
                false
            } else {
                true
            }
        });
        announced_outside.retain(|&epoch| epoch >= keep_from);
        announced_waiting.retain(|&epoch| epoch >= keep_from);
        announced_unsettled.retain(|&epoch| epoch >= keep_from);

        // Prune consensus storage past the retention horizon. Only epochs the
        // chain has fully moved beyond are eligible, and the rotation never
        // starts engines below the tail again — a pruned epoch's journal has no
        // reader left. Restarts re-walk from zero: removing an already-missing
        // partition is a no-op, so the walk is idempotent.
        if let Some(retention) = epoch_retention {
            let horizon = keep_from.saturating_sub(retention.get());
            if horizon > pruned_below {
                for epoch in pruned_below..horizon {
                    match context
                        .remove(&engine_partition(&partition_prefix, epoch), None)
                        .await
                    {
                        Ok(()) => info!(epoch, "pruned a retired epoch's vote journal"),
                        // Never created (a member that joined later) or already
                        // pruned before a restart — nothing to do either way.
                        Err(commonware_runtime::Error::PartitionMissing(_)) => {}
                        Err(err) => {
                            tracing::warn!(
                                ?err,
                                epoch,
                                "failed to prune a retired epoch's vote journal"
                            );
                        }
                    }
                }
                // Marshal's finalized block/certificate archives, below the
                // horizon epoch's anchor. Upstream refuses to prune above its
                // processed floor, so this can never outrun block delivery. The
                // node's own finality store is deliberately not part of this:
                // certificates there are the permanent proof trail.
                if let Some(first) = epocher.first(Epoch::new(horizon)) {
                    marshal.prune(Height::new(first.get().saturating_sub(1)));
                }
                pruned_below = horizon;
            }
        }

        context.sleep(EPOCH_POLL).await;
    }
}
