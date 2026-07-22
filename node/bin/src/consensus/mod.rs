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

mod gossip;
mod lifecycle;
mod reporting;
mod setup;
#[cfg(test)]
mod tests;

use gossip::start_tx_gossip;
use lifecycle::select_stack_start;
use reporting::{ActivityObserver, registry_status};

pub use lifecycle::{
    EraDecision, check_rollback_acknowledged, decide_consensus_era, instance_lock_path,
    parse_acknowledge_fork, read_truncation_flag, truncation_flag_path,
};
pub use reporting::ConsensusObservability;
pub use setup::{
    CommitteeMember, ConsensusSetup, ObserverPeer, RegistrySetup, parse_committee_member,
    parse_observer_peer,
};

use anyhow::Context as _;
use commonware_cryptography::ed25519;
use commonware_p2p::authenticated::lookup;
use commonware_p2p::{Address, AddressableManager as _, Ingress};
use commonware_runtime::{Metrics as _, Quota, Runner as _, Spawner as _, Supervisor as _};
use commonware_utils::TryCollect as _;
use commonware_utils::ordered::Map;
use commonware_utils::union_unique;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use zksync_os_consensus_core::{Channels, StackConfig, start_validator};
use zksync_os_consensus_execution::NodeExecutionEnv;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_storage_api::{ReadStateHistory, WriteState};

/// Stack size for the consensus thread and the consensus runtime's worker
/// threads. Proposal building and verification re-execute blocks on these
/// threads, and execution's call stacks run deep — past the 2 MiB default of a
/// spawned thread, which overflows on a bare production binary. (Processes run
/// through cargo are covered by the `RUST_MIN_STACK` in `.cargo/config.toml`;
/// this constant matches its value.)
const CONSENSUS_STACK_SIZE: usize = 16 * 1024 * 1024;

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
        .stack_size(CONSENSUS_STACK_SIZE)
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
        .with_thread_stack_size(CONSENSUS_STACK_SIZE)
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
            max_message_size: setup.max_message_size.get(),
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
