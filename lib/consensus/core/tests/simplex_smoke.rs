//! "Hello world" for the raw commonware simplex engine, below our own integration
//! layers (no marshal, no block dissemination — digests only).
//!
//! What this proves (and is kept as a permanent regression test for):
//! 1. The pinned commonware version compiles and runs inside this workspace.
//! 2. We can wire a `simplex::Engine` end-to-end: certificate scheme, leader election,
//!    the three p2p channels, journal partitions, and the `Automaton`/`Relay`/`Reporter`
//!    integration traits.
//! 3. Five validators finalize views over a simulated network, with no forks, no
//!    Byzantine-fault evidence, and no peers blocked.
//! 4. The deterministic runtime is bit-exact: the same seed reproduces the exact same
//!    execution (compared via the runtime's auditor hash), and a different seed does not.
//!
//! The "chain" here is trivial on purpose: payloads are just hashes derived from the
//! consensus round, `verify` accepts everything, and nothing is executed or stored.
//! The real integration (full blocks, verification, dissemination, ordered delivery)
//! lives in the crate itself and is exercised by the simulation crate's cluster tests.

use commonware_actor::Feedback;
use commonware_consensus::simplex::scheme::bls12381_multisig;
use commonware_consensus::simplex::types::{Activity, Context};
use commonware_consensus::simplex::{
    Engine, Plan, config::Config as EngineConfig, config::Floor, config::ForwardingPolicy,
    elector::RoundRobin,
};
use commonware_consensus::types::{Epoch, View, ViewDelta};
use commonware_consensus::{Automaton, CertifiableAutomaton, Relay, Reporter, Viewable};
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::certificate::mocks::Fixture;
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::sha256::Digest as Sha256Digest;
use commonware_cryptography::{Hasher, Sha256};
use commonware_p2p::simulated::{Config as NetworkConfig, Link, Network, Oracle};
use commonware_parallel::Sequential;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_runtime::{Clock, Quota, Runner, Supervisor as _, deterministic};
use commonware_utils::channel::oneshot;
use commonware_utils::{NZU16, NZUsize};
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The same certificate scheme the production stack uses; the rationale lives on
/// [`zksync_os_consensus_core::types::Scheme`].
type MultisigScheme = zksync_os_consensus_core::types::Scheme;

const NUM_VALIDATORS: u32 = 5;
/// Views every validator must finalize before the test passes.
const TARGET_VIEW: u64 = 50;
/// Domain-separation namespace for all consensus signatures in this test.
const NAMESPACE: &[u8] = b"zksync-os-simplex-smoke";

/// The simplest possible consensus application:
/// - `propose` derives a payload digest from the round and parent (no real block),
/// - `verify` accepts any payload,
/// - `broadcast` does nothing (there is no payload data to disseminate).
///
/// Consensus itself never sees more than this digest — full payload dissemination is the
/// application's job (marshal + buffered broadcast in the real integration).
#[derive(Clone)]
struct TrivialAutomaton;

impl Automaton for TrivialAutomaton {
    type Context = Context<Sha256Digest, PublicKey>;
    type Digest = Sha256Digest;

    async fn propose(&mut self, context: Self::Context) -> oneshot::Receiver<Self::Digest> {
        // The engine consumes the digest through a oneshot so a slow application can never
        // block consensus: if this channel stayed pending past the leader timeout, the view
        // would simply nullify. Our "payload building" is instant.
        let (response, receiver) = oneshot::channel();
        let mut hasher = Sha256::new();
        hasher.update(&context.round.view().get().to_be_bytes());
        hasher.update(context.parent.1.as_ref());
        let _ = response.send(hasher.finalize());
        receiver
    }

    async fn verify(
        &mut self,
        _context: Self::Context,
        _payload: Self::Digest,
    ) -> oneshot::Receiver<bool> {
        // Everything is valid in the smoke test. The real application answers `false` only
        // for permanent invalidity and stays pending while payload data is still in flight.
        let (response, receiver) = oneshot::channel();
        let _ = response.send(true);
        receiver
    }
}

// Default `certify` immediately approves every notarized payload; the real application can
// use this hook to delay finalization votes (e.g. until execution catches up).
impl CertifiableAutomaton for TrivialAutomaton {}

impl Relay for TrivialAutomaton {
    type Digest = Sha256Digest;
    type PublicKey = PublicKey;
    type Plan = Plan<PublicKey>;

    fn broadcast(&mut self, _payload: Self::Digest, _plan: Self::Plan) -> Feedback {
        // No payload bodies exist in this test, so there is nothing to disseminate.
        Feedback::Ok
    }
}

/// Records what the engine reports so the test can assert on it afterwards.
///
/// The engine reports every activity it observes: votes, recovered certificates, and
/// Byzantine fault evidence. We track finalized payloads per view (fork detection) and
/// count fault evidence (must be zero in an honest cluster).
#[derive(Clone, Default)]
struct ActivityLog {
    inner: Arc<Mutex<ActivityLogInner>>,
}

#[derive(Default)]
struct ActivityLogInner {
    /// View -> the payload digest carried by the recovered finalization certificate.
    finalizations: BTreeMap<View, Sha256Digest>,
    /// Byzantine fault evidence observed (conflicting votes etc.). Must stay zero here.
    faults: usize,
}

impl ActivityLog {
    fn highest_finalized_view(&self) -> Option<View> {
        self.inner
            .lock()
            .unwrap()
            .finalizations
            .last_key_value()
            .map(|(view, _)| *view)
    }

    fn finalizations(&self) -> BTreeMap<View, Sha256Digest> {
        self.inner.lock().unwrap().finalizations.clone()
    }

    fn faults(&self) -> usize {
        self.inner.lock().unwrap().faults
    }
}

impl Reporter for ActivityLog {
    type Activity = Activity<MultisigScheme, Sha256Digest>;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        let mut inner = self.inner.lock().unwrap();
        match activity {
            Activity::Finalization(finalization) => {
                inner
                    .finalizations
                    .insert(finalization.view(), finalization.proposal.payload);
            }
            Activity::ConflictingNotarize(_)
            | Activity::ConflictingFinalize(_)
            | Activity::NullifyFinalize(_) => {
                inner.faults += 1;
            }
            // Individual votes, notarizations, nullifications: not needed for these
            // assertions (the real integration feeds them to marshal and metrics).
            _ => {}
        }
        Feedback::Ok
    }
}

/// Runs a full cluster to `TARGET_VIEW` finalized views and returns the runtime's auditor
/// hash — a fingerprint of the entire execution, used for determinism assertions.
fn run_cluster(seed: u64) -> String {
    // Virtual-time budget: the runtime panics if the cluster needs more than this to
    // finalize TARGET_VIEW views — a loud liveness failure instead of a hang.
    let config = deterministic::Config::new()
        .with_seed(seed)
        .with_timeout(Some(Duration::from_secs(120)));
    let runner = deterministic::Runner::new(config);

    runner.start(|mut context| async move {
        // One BLS keypair + one scheme instance per validator, plus the ordered
        // participant set shared by all of them. In production these come from the
        // validator-set configuration; here commonware's test fixture generates them.
        let Fixture {
            participants,
            schemes,
            ..
        } = bls12381_multisig::fixture::<MinPk, _>(&mut context, NAMESPACE, NUM_VALIDATORS);

        // Simulated p2p network. The oracle is the test's god-handle: it registers
        // per-validator channels and controls link quality between peers.
        let (network, mut oracle) = Network::new_with_peers(
            context.child("network"),
            NetworkConfig {
                max_size: 1024 * 1024,
                disconnect_on_block: true,
                tracked_peer_sets: NZUsize!(1),
            },
            participants.clone(),
        )
        .await;
        network.start();

        // Healthy, symmetric links between every pair of validators.
        add_full_mesh(
            &mut oracle,
            &participants,
            Link {
                latency: Duration::from_millis(10),
                jitter: Duration::from_millis(1),
                success_rate: 1.0,
            },
        )
        .await;

        // One engine per validator, all in the same (only) epoch. Every engine starts
        // from the same agreed genesis digest — the engine's `floor` (since 2026.5.0
        // the starting payload lives in the config, not on the `Automaton`).
        let epoch = Epoch::new(1);
        let genesis = Sha256::hash(b"smoke-genesis");
        let mut logs = Vec::new();
        for (index, validator) in participants.iter().enumerate() {
            let context = context
                .child("validator")
                .with_attribute("index", index.to_string());

            // The engine talks to peers over three dedicated channels:
            // 0 = individual votes, 1 = recovered certificates, 2 = certificate backfill.
            let quota = Quota::per_second(NonZeroU32::MAX);
            let control = oracle.control(validator.clone());
            let votes = control.register(0, quota).await.unwrap();
            let certificates = control.register(1, quota).await.unwrap();
            let resolver = control.register(2, quota).await.unwrap();

            let log = ActivityLog::default();
            logs.push(log.clone());

            let automaton = TrivialAutomaton;
            let engine_config = EngineConfig {
                scheme: schemes[index].clone(),
                elector: RoundRobin::<Sha256>::default(),
                blocker: oracle.control(validator.clone()),
                automaton: automaton.clone(),
                relay: automaton,
                reporter: log,
                strategy: Sequential,
                // The journal namespace. Must be unique per validator (and per epoch, once
                // we run more than one): it is where the engine fsyncs every vote before
                // broadcasting it, which is what prevents double-signing after a crash.
                partition: format!("validator_{index}"),
                mailbox_size: NZUsize!(1024),
                epoch,
                // Where this epoch's chain starts: the genesis payload every validator
                // derives identically. (The production stack passes the epoch's anchor
                // block digest here.)
                floor: Floor::Genesis(genesis),
                replay_buffer: NZUsize!(1024 * 1024),
                write_buffer: NZUsize!(1024 * 1024),
                page_cache: CacheRef::from_pooler(&context, NZU16!(1024), NZUsize!(10)),
                leader_timeout: Duration::from_secs(1),
                certification_timeout: Duration::from_secs(2),
                timeout_retry: Duration::from_secs(10),
                activity_timeout: ViewDelta::new(10),
                skip_timeout: ViewDelta::new(5),
                fetch_timeout: Duration::from_secs(1),
                fetch_concurrent: NZUsize!(4),
                forwarding: ForwardingPolicy::Disabled,
            };
            let engine = Engine::new(context.child("engine"), engine_config);
            engine.start(votes, certificates, resolver);
        }

        // Wait (in virtual time — this is instant in wall-clock terms) until every
        // validator has observed a finalization at or beyond the target view.
        let target = View::new(TARGET_VIEW);
        loop {
            context.sleep(Duration::from_millis(100)).await;
            let all_reached = logs
                .iter()
                .all(|log| log.highest_finalized_view() >= Some(target));
            if all_reached {
                break;
            }
        }

        // Safety: no two validators may ever finalize different payloads for the same
        // view. Compare every validator's log against the first one's.
        let reference = logs[0].finalizations();
        for (index, log) in logs.iter().enumerate().skip(1) {
            for (view, digest) in log.finalizations() {
                if let Some(reference_digest) = reference.get(&view) {
                    assert_eq!(
                        *reference_digest, digest,
                        "validator_{index} finalized a different payload at view {view}",
                    );
                }
            }
        }

        // Cleanliness: honest validators must produce no Byzantine fault evidence, and the
        // network oracle must not have blocked anyone.
        for (index, log) in logs.iter().enumerate() {
            assert_eq!(log.faults(), 0, "validator_{index} observed fault evidence");
        }
        let blocked = oracle.blocked().await.unwrap();
        assert!(blocked.is_empty(), "peers were blocked: {blocked:?}");

        context.auditor().state()
    })
}

/// Symmetrically links every pair of distinct validators with the same link quality.
async fn add_full_mesh(
    oracle: &mut Oracle<PublicKey, deterministic::Context>,
    validators: &[PublicKey],
    link: Link,
) {
    for a in validators {
        for b in validators {
            if a == b {
                continue;
            }
            oracle
                .add_link(a.clone(), b.clone(), link.clone())
                .await
                .unwrap();
        }
    }
}

#[test]
fn five_validators_finalize() {
    run_cluster(42);
}

#[test]
fn same_seed_reproduces_the_exact_same_execution() {
    // The auditor hash folds every scheduler decision, rng draw, and network event into
    // one fingerprint. Equal hashes = bit-exact reproduction. This is the assertion style
    // our whole deterministic test suite will rely on, so it is proven here first.
    let first = run_cluster(7);
    let second = run_cluster(7);
    assert_eq!(first, second, "same seed must reproduce the same execution");

    // Sanity check that the fingerprint actually captures the execution: a different seed
    // must produce a different interleaving.
    let other = run_cluster(8);
    assert_ne!(first, other, "different seeds should diverge");
}
