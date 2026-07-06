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
use commonware_broadcast::buffered;
use commonware_consensus::marshal::standard::{Inline, Standard};
use commonware_consensus::marshal::{self, core as marshal_core, resolver};
use commonware_consensus::simplex::config::ForwardingPolicy;
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
    pub mailbox_size: usize,
    /// How many recently-gossiped blocks the broadcast cache keeps per peer.
    pub deque_size: usize,
    /// How many finalized blocks may be in flight to the committer before marshal stops
    /// delivering more (the execution side is the pacer).
    pub max_pending_acks: NonZeroUsize,
    /// How long marshal retains per-view scratch data after processing.
    pub view_retention_timeout: ViewDelta,
    /// Cap on concurrent backfill repairs.
    pub max_repair: NonZeroUsize,
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
            mailbox_size: 1024,
            deque_size: 16,
            max_pending_acks: NonZeroUsize::new(4).expect("nonzero"),
            view_retention_timeout: ViewDelta::new(100),
            max_repair: NonZeroUsize::new(16).expect("nonzero"),
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

    async fn report(&mut self, _activity: Self::Activity) {}
}

/// The engines currently running, one per live epoch — shared between the epoch
/// rotation task (which starts and retires them) and [`ValidatorStack::abort`]
/// (which must kill whatever is alive when the whole validator stops).
pub type EngineRegistry = Arc<Mutex<BTreeMap<u64, Handle<()>>>>;

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
) -> ValidatorStack<X::Block>
where
    R: Clock + Spawner + Metrics + Storage + BufferPooler + Rng + CryptoRng + Clone,
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
        context.with_label("block_broadcast"),
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
        &context,
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

    let finalizations =
        init_finalizations_archive(&context, &config.partition_prefix, page_cache.clone()).await;
    let blocks = init_blocks_archive::<_, X::Block>(
        &context,
        &config.partition_prefix,
        page_cache.clone(),
        block_codec_config.clone(),
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
    let (marshal_actor, marshal_mailbox, marshal_height) = marshal_core::Actor::init(
        context.with_label("marshal"),
        finalizations,
        blocks,
        marshal::Config {
            // Marshal verifies certificates from arbitrary historical epochs during
            // backfill — the provider resolves each epoch to its committee's scheme.
            provider: scheme_provider.clone(),
            epocher: epocher.clone(),
            partition_prefix: config.partition_prefix.clone(),
            mailbox_size: config.mailbox_size,
            view_retention_timeout: config.view_retention_timeout,
            prunable_items_per_section: NonZeroU64::new(1024).expect("nonzero"),
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

    // If the node has already durably applied blocks beyond what the archives hold
    // (e.g. the archives were pruned or lost while the node state survived), start
    // delivery from the node's height instead of replaying ancient history.
    if let Some(committed) = env.committed_height().await
        && committed > marshal_height
    {
        marshal_mailbox.set_floor(committed).await;
    }

    let committer = FinalizedBlockCommitter::new(env.clone());
    let marshal = marshal_actor.start(committer, broadcast_mailbox, block_resolver);

    // The application half: judge and build blocks. `Inline` = full verification happens
    // before this validator votes, never after.
    let application = Inline::new(
        context.with_label("application"),
        ExecutionApplication::new(env.clone()),
        marshal_mailbox.clone(),
        epocher.clone(),
    );

    // The engine channels are multiplexed by epoch id: during an epoch handoff two
    // engines are briefly alive at once (the old one finalizing its tail, the new
    // one re-certifying the boundary block), and each must see only its own epoch's
    // traffic. Messages for epochs nobody runs anymore are dropped by the muxer.
    let (votes_muxer, votes_mux) = Muxer::new(
        context.with_label("votes_mux"),
        channels.votes.0,
        channels.votes.1,
        config.mailbox_size,
    );
    let (certificates_muxer, certificates_mux, certificate_backup) = {
        use commonware_p2p::utils::mux::Builder as _;
        Muxer::builder(
            context.with_label("certificates_mux"),
            channels.certificates.0,
            channels.certificates.1,
            config.mailbox_size,
        )
        .with_backup()
        .build()
    };
    let (certificate_backfill_muxer, certificate_backfill_mux) = Muxer::new(
        context.with_label("certificate_backfill_mux"),
        channels.certificate_backfill.0,
        channels.certificate_backfill.1,
        config.mailbox_size,
    );
    let mut support_tasks = vec![
        // A mux only exits when its underlying channel dies, which means the p2p
        // network died — the node watches the network task itself, so these only
        // need to leave a trace.
        context.with_label("votes_mux_task").spawn(|_| async move {
            if let Err(err) = votes_muxer.run().await {
                tracing::error!(?err, "votes mux exited");
            }
        }),
        context
            .with_label("certificates_mux_task")
            .spawn(|_| async move {
                if let Err(err) = certificates_muxer.run().await {
                    tracing::error!(?err, "certificates mux exited");
                }
            }),
        context
            .with_label("certificate_backfill_mux_task")
            .spawn(|_| async move {
                if let Err(err) = certificate_backfill_muxer.run().await {
                    tracing::error!(?err, "certificate backfill mux exited");
                }
            }),
    ];

    // Certificates for epochs this validator runs flow to their engines through the
    // mux; certificates for epochs it does NOT run land on the backup lane. One of
    // them is exactly how a validator that slept through epoch boundaries discovers
    // the chain moved on: a *valid* finalization from a later epoch is self-proving
    // evidence of the real tip, no matter which peer sent it or what its message was
    // tagged with. Verify it, hand it to marshal — marshal backfills the gap, the
    // committed height advances, and the epoch rotation follows it forward. Without
    // this, a validator more than one epoch behind would wait forever: its old
    // engine hears nothing (peers retired that epoch), and the current epoch's
    // traffic would be dropped unheard.
    let tip_scout = context.with_label("tip_scout").spawn({
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
                scout_reporter
                    .report(Activity::Finalization(finalization.clone()))
                    .await;
                marshal_mailbox
                    .report(Activity::Finalization(finalization))
                    .await;
            }
        }
    });

    // Everything one engine needs, captured once; the rotation task calls this for
    // each epoch it starts — and only for epochs where this validator is a committee
    // member (the rotation task checks membership before calling). The closure keeps
    // `start_validator` the single place where an engine's configuration is spelled
    // out.
    let spawn_engine = {
        let context = context.clone();
        let config = config.clone();
        let scheme_provider = scheme_provider.clone();
        let marshal_mailbox = marshal_mailbox.clone();
        move |epoch: Epoch,
              votes: (SubSender<TSender>, SubReceiver<TReceiver>),
              certificates: (SubSender<TSender>, SubReceiver<TReceiver>),
              certificate_backfill: (SubSender<TSender>, SubReceiver<TReceiver>)|
              -> Handle<()> {
            let engine = Engine::new(
                context.with_label(&format!("engine_epoch_{}", epoch.get())),
                EngineConfig {
                    scheme: scheme_provider.scheme_for(epoch).as_ref().clone(),
                    elector: Elector::default(),
                    blocker: blocker.clone(),
                    automaton: application.clone(),
                    relay: application.clone(),
                    // Marshal consumes consensus outcomes (certificates drive
                    // finalization); the extra reporter observes alongside it.
                    reporter: Reporters::from((marshal_mailbox.clone(), extra_reporter.clone())),
                    strategy: Sequential,
                    partition: format!("{}-engine-epoch-{}", config.partition_prefix, epoch.get()),
                    mailbox_size: config.mailbox_size,
                    epoch,
                    replay_buffer: NZUsize!(1024 * 1024),
                    write_buffer: NZUsize!(1024 * 1024),
                    page_cache: page_cache.clone(),
                    leader_timeout: config.leader_timeout,
                    certification_timeout: config.certification_timeout,
                    timeout_retry: config.timeout_retry,
                    activity_timeout: config.activity_timeout,
                    skip_timeout: config.skip_timeout,
                    fetch_timeout: config.fetch_timeout,
                    fetch_concurrent: 4,
                    forwarding: ForwardingPolicy::Disabled,
                },
            );
            engine.start(votes, certificates, certificate_backfill)
        }
    };

    let engines: EngineRegistry = Arc::new(Mutex::new(BTreeMap::new()));
    let epoch_manager = context.with_label("epoch_manager").spawn({
        let engines = engines.clone();
        move |context| {
            run_epoch_rotation(
                context,
                epocher,
                scheme_provider,
                env,
                votes_mux,
                certificates_mux,
                certificate_backfill_mux,
                engines,
                spawn_engine,
            )
        }
    });

    support_tasks.push(tip_scout);

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
    mut votes_mux: MuxHandle<TSender, TReceiver>,
    mut certificates_mux: MuxHandle<TSender, TReceiver>,
    mut certificate_backfill_mux: MuxHandle<TSender, TReceiver>,
    engines: EngineRegistry,
    spawn_engine: F,
) where
    R: Clock + Spawner + Metrics + Clone,
    X: ExecutionEnv,
    TSender: commonware_p2p::Sender<PublicKey = PublicKey>,
    TReceiver: commonware_p2p::Receiver<PublicKey = PublicKey>,
    F: Fn(
        Epoch,
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
            let votes = votes_mux.register(key).await.expect("register votes mux");
            let certificates = certificates_mux
                .register(key)
                .await
                .expect("register certificates mux");
            let certificate_backfill = certificate_backfill_mux
                .register(key)
                .await
                .expect("register certificate backfill mux");
            info!(epoch = key, "starting consensus engine for epoch");
            let handle = spawn_engine(epoch, votes, certificates, certificate_backfill);
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

        context.sleep(EPOCH_POLL).await;
    }
}
