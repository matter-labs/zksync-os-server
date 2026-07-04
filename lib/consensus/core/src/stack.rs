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
//! One stack instance = one validator in one epoch. With the static validator set there
//! is a single epoch (number 0) whose length is effectively unbounded; committee rotation
//! later means starting a new engine per epoch while marshal and the broadcast layer keep
//! running.

use crate::application::ExecutionApplication;
use crate::committer::FinalizedBlockCommitter;
use crate::execution::ExecutionEnv;
use crate::storage::{init_blocks_archive, init_finalizations_archive};
use crate::types::{Elector, Scheme, SchemeProvider};
use commonware_broadcast::buffered;
use commonware_consensus::marshal::standard::{Inline, Standard};
use commonware_consensus::marshal::{self, core as marshal_core, resolver};
use commonware_consensus::simplex::config::ForwardingPolicy;
use commonware_consensus::simplex::types::Activity;
use commonware_consensus::simplex::{Engine, config::Config as EngineConfig};
use commonware_consensus::types::{Epoch, FixedEpocher, ViewDelta};
use commonware_consensus::{Reporter, Reporters};
use commonware_cryptography::Digestible;
use commonware_cryptography::ed25519::PublicKey;
use commonware_parallel::Sequential;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_runtime::{BufferPooler, Clock, Handle, Metrics, Spawner, Storage};
use commonware_utils::{NZU16, NZUsize};
use rand08::{CryptoRng, Rng};
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

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
            // A single effectively-unbounded epoch: one billion blocks is decades of
            // production at sub-second block times, while staying far from any overflow.
            // TODO(consensus): real epoch rotation is the mechanism for validator-set
            // changes and for bounding consensus-archive growth — measure storage churn
            // in staging and revisit before long-lived deployments.
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

/// Handles to a running validator stack. Aborting all handles stops the validator;
/// starting a fresh stack with the same `partition_prefix` over the same storage
/// resumes it (journal replay restores consensus state without double-signing).
pub struct ValidatorStack<B: commonware_consensus::Block> {
    pub engine: Handle<()>,
    pub marshal: Handle<()>,
    pub broadcast: Handle<()>,
    /// Query surface into marshal (finalized tip, blocks by height/digest).
    pub marshal_mailbox: marshal_core::Mailbox<Scheme, Standard<B>>,
}

impl<B: commonware_consensus::Block> ValidatorStack<B> {
    /// Stops all components. The stack can be started again over the same storage.
    pub fn abort(&self) {
        self.engine.abort();
        self.marshal.abort();
        self.broadcast.abort();
    }
}

/// Builds and starts every component of one validator: broadcast, backfill, archives,
/// marshal, the application adapter, and the simplex engine for epoch 0.
#[allow(clippy::too_many_arguments)]
pub async fn start_validator<R, X, TSender, TReceiver, TBlocker, TPeers, TReporter>(
    context: R,
    config: StackConfig,
    identity: PublicKey,
    scheme: Scheme,
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

    let epocher = FixedEpocher::new(config.epoch_length);
    let (marshal_actor, marshal_mailbox, marshal_height) = marshal_core::Actor::init(
        context.with_label("marshal"),
        finalizations,
        blocks,
        marshal::Config {
            provider: SchemeProvider::new(scheme.clone()),
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
        ExecutionApplication::new(env),
        marshal_mailbox.clone(),
        epocher,
    );

    let epoch = Epoch::new(0);
    let engine = Engine::new(
        context.with_label("engine"),
        EngineConfig {
            scheme,
            elector: Elector::default(),
            blocker,
            automaton: application.clone(),
            relay: application,
            // Marshal consumes consensus outcomes (certificates drive finalization);
            // the extra reporter observes alongside it.
            reporter: Reporters::from((marshal_mailbox.clone(), extra_reporter)),
            strategy: Sequential,
            partition: format!("{}-engine-epoch-{}", config.partition_prefix, epoch.get()),
            mailbox_size: config.mailbox_size,
            epoch,
            replay_buffer: NZUsize!(1024 * 1024),
            write_buffer: NZUsize!(1024 * 1024),
            page_cache,
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
    let engine = engine.start(
        channels.votes,
        channels.certificates,
        channels.certificate_backfill,
    );

    ValidatorStack {
        engine,
        marshal,
        broadcast,
        marshal_mailbox,
    }
}
