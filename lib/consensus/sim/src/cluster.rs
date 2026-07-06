//! A simulated validator cluster: N full consensus stacks over an in-memory network,
//! inside the deterministic runtime.
//!
//! Everything a test needs in one handle: start a cluster, shape the network (partitions,
//! degraded links), crash and restart validators, plant byzantine ones, and assert on
//! what was committed. Time is virtual (a minute of consensus takes milliseconds of wall
//! clock) and every run is reproducible from the runtime seed.
//!
//! The cluster is generic over the execution backend: the same harness drives the
//! in-memory [`MockExecution`] (fast, content-free blocks) and the real-STF environment
//! (actual transactions executed by the production VM). `SimCluster` defaults to the
//! mock so most scenarios stay short.

use crate::activity::ActivityLog;
use crate::execution::{MockExecution, SimEnv};
use commonware_codec::{DecodeExt as _, Read};
use commonware_consensus::simplex::mocks::{conflicter, nuller};
use commonware_consensus::types::Epoch;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::ops::compute_public;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::sha256::Digest as Sha256Digest;
use commonware_cryptography::{Digestible, Sha256, Signer as _, ed25519};
use commonware_p2p::simulated::{Config as NetworkConfig, Link, Network, Oracle};
use commonware_runtime::{Clock, Handle, Metrics, Quota, Spawner as _, deterministic};
use commonware_utils::{NZUsize, TryFromIterator as _};
use rand08::Rng as _;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_consensus_core::types::{Scheme, SchemeProvider};
use zksync_os_consensus_core::{
    Channels, CommitteeSchedule, ScheduleEntry, StackConfig, ValidatorStack, start_validator,
};

/// Adjusts the stack configuration every validator (and every restart) is built
/// with — how scenarios opt into short epochs or unusual timeouts without the
/// cluster growing a parameter per knob.
pub type StackTuner = Arc<dyn Fn(&mut StackConfig) + Send + Sync>;

/// Committee membership per activation epoch, by validator index — the sim-side
/// spelling of the committee schedule. An empty spec means the default: one
/// committee of every validator, holding all epochs. Reconfiguration scenarios
/// list explicit entries, e.g. `[(0, vec![0, 1, 2]), (2, vec![0, 1, 2, 3])]`
/// grows the committee at epoch 2.
pub type ScheduleSpec = Vec<(u64, Vec<usize>)>;

/// The tail options of [`SimCluster::start_era`], with workable defaults — so
/// scenarios name only what they bend.
pub struct EraOptions {
    /// Validators that are provisioned (keys, membership, environment) but not
    /// started — the late-deploy case; [`SimCluster::restart`] brings one up.
    pub stopped: Vec<usize>,
    /// Namespaces every validator's journals/archives within the run's shared
    /// storage. Scenarios starting a *second* cluster in one run pass a distinct
    /// prefix, or the first era's storage would silently be reopened.
    pub storage_prefix: String,
    pub stack_tuner: StackTuner,
    /// The committee schedule everyone agrees on (empty: everyone, from epoch 0).
    pub schedule: ScheduleSpec,
    /// Per-validator disagreements with `schedule` — the operator-error scenarios
    /// (a validator whose config is missing the newest entry). Each listed
    /// validator builds its scheme provider from its own spec instead.
    pub schedule_overrides: Vec<(usize, ScheduleSpec)>,
}

impl Default for EraOptions {
    fn default() -> Self {
        Self {
            stopped: Vec::new(),
            storage_prefix: "validator".to_string(),
            stack_tuner: Arc::new(|_| {}),
            schedule: Vec::new(),
            schedule_overrides: Vec::new(),
        }
    }
}

/// Domain-separation namespace for all consensus signatures in simulation.
const NAMESPACE: &[u8] = b"zksync-os-consensus-sim";

/// Channel ids on the simulated network, one per consensus traffic class.
const VOTES: u64 = 0;
const CERTIFICATES: u64 = 1;
const CERTIFICATE_BACKFILL: u64 = 2;
const BLOCK_BROADCAST: u64 = 3;
const BLOCK_BACKFILL: u64 = 4;

/// How a validator behaves in the scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Runs the real consensus stack.
    Honest,
    /// Byzantine: signs two conflicting notarize/finalize votes per view — the classic
    /// equivocation attack that BFT consensus must detect and tolerate.
    Conflicter,
    /// Byzantine: votes both to accept (notarize/finalize) and to skip (nullify) the
    /// same view.
    Nuller,
}

/// What is currently running for a validator.
enum Running<B: commonware_consensus::Block> {
    Full(ValidatorStack<B>),
    /// The byzantine engine plus the channel mux it attacks through.
    Byzantine(Vec<Handle<()>>),
}

pub struct SimCluster<X: SimEnv = MockExecution> {
    context: deterministic::Context,
    pub oracle: Oracle<PublicKey, deterministic::Context>,
    pub validators: Vec<SimValidator<X>>,
    stack_tuner: StackTuner,
    /// Every validator's keys (sorted by network identity, aligned with
    /// `validators`) — kept so schedules can be re-derived for reconfiguration.
    keys: Vec<(ed25519::PrivateKey, group::Private)>,
}

pub struct SimValidator<X: SimEnv> {
    pub identity: PublicKey,
    pub behavior: Behavior,
    /// This validator's view of the chain; survives crash/restart like a node's disk.
    /// Stays empty for byzantine validators (they run no execution).
    pub env: X,
    /// Records fault evidence this validator observed.
    pub activity: ActivityLog,
    running: Option<Running<X::Block>>,
    /// Per-epoch schemes over the run's committee schedule, with this validator's
    /// signing key. Shared shape with production; byzantine mocks pull their
    /// signing scheme out of it for the epoch they attack.
    provider: SchemeProvider,
    partition_prefix: String,
    /// How many times this validator has been started. Only used to give each
    /// incarnation a distinct metrics namespace: in production a restart is a fresh
    /// process with a fresh metrics registry, but in simulation the registry lives as
    /// long as the run, and registering the same metric twice is an error.
    incarnation: usize,
}

impl SimCluster<MockExecution> {
    /// Starts `num_validators` honest, fully-linked validators over mock execution.
    pub async fn start(context: deterministic::Context, num_validators: u32, link: Link) -> Self {
        let behaviors = vec![Behavior::Honest; num_validators as usize];
        Self::start_with_behaviors(context, &behaviors, link).await
    }

    /// Starts one validator per entry in `behaviors` over mock execution.
    pub async fn start_with_behaviors(
        context: deterministic::Context,
        behaviors: &[Behavior],
        link: Link,
    ) -> Self {
        Self::start_with_env(context, behaviors, link, |_index, _context| {
            MockExecution::new()
        })
        .await
    }
}

impl<X> SimCluster<X>
where
    X: SimEnv,
    X::Block: Digestible<Digest = Sha256Digest>,
    <X::Block as Read>::Cfg: Default + Clone + Send + Sync + 'static,
{
    /// Starts one validator per entry in `behaviors`, fully linked, each with the
    /// execution environment `env_factory` builds for it. Byzantine validators join the
    /// network with valid credentials — they are real committee members whose key
    /// happens to sign contradictory votes.
    pub async fn start_with_env(
        context: deterministic::Context,
        behaviors: &[Behavior],
        link: Link,
        env_factory: impl Fn(usize, deterministic::Context) -> X,
    ) -> Self {
        Self::start_with_env_stopped(context, behaviors, link, env_factory, &[]).await
    }

    /// Like [`Self::start_with_env`], but the validators listed in `stopped` are only
    /// *provisioned* — they have keys, committee membership, and an execution
    /// environment, but no running stack and no storage history. [`Self::restart`]
    /// brings one up later. This models a validator that is part of the committee from
    /// genesis but deploys long after the chain started — the late-join case.
    pub async fn start_with_env_stopped(
        context: deterministic::Context,
        behaviors: &[Behavior],
        link: Link,
        env_factory: impl Fn(usize, deterministic::Context) -> X,
        stopped: &[usize],
    ) -> Self {
        Self::start_era(
            context,
            behaviors,
            link,
            env_factory,
            EraOptions {
                stopped: stopped.to_vec(),
                ..EraOptions::default()
            },
        )
        .await
    }

    /// The fullest constructor; see [`EraOptions`] for the knobs.
    pub async fn start_era(
        mut context: deterministic::Context,
        behaviors: &[Behavior],
        link: Link,
        env_factory: impl Fn(usize, deterministic::Context) -> X,
        options: EraOptions,
    ) -> Self {
        let EraOptions {
            stopped,
            storage_prefix,
            stack_tuner,
            schedule: schedule_spec,
            schedule_overrides,
        } = options;
        // Mint the validators' keys from the seeded runtime — deterministic per
        // seed, and the same construction path production uses (no fixture): each
        // validator holds an ed25519 network identity and a BLS signing key.
        let mut keys: Vec<(ed25519::PrivateKey, group::Private)> = (0..behaviors.len())
            .map(|_| {
                let seed: [u8; 32] = context.r#gen();
                let network =
                    ed25519::PrivateKey::decode(seed.as_slice()).expect("32 bytes are a seed");
                // BLS scalars must be canonical; retry until the bytes decode.
                let bls = loop {
                    let bytes: [u8; 32] = context.r#gen();
                    if let Ok(key) = group::Private::decode(bytes.as_slice()) {
                        break key;
                    }
                };
                (network, bls)
            })
            .collect();
        // Deterministic index ↔ identity mapping regardless of key values (peer
        // registration and leader rotation see sorted committees either way).
        keys.sort_by_key(|(network, _)| network.public_key());
        let participants: Vec<PublicKey> = keys
            .iter()
            .map(|(network, _)| network.public_key())
            .collect();

        let schedule = Self::build_schedule(&keys, &schedule_spec, behaviors.len());

        let (network, oracle) = Network::new_with_peers(
            context.with_label("network"),
            NetworkConfig {
                max_size: 1024 * 1024,
                disconnect_on_block: true,
                tracked_peer_sets: NZUsize!(1),
            },
            participants.clone(),
        )
        .await;
        network.start();

        let mut cluster = Self {
            context,
            oracle,
            validators: Vec::new(),
            stack_tuner,
            keys,
        };
        cluster.link_full_mesh_between(&participants, link).await;

        for (index, identity) in participants.iter().enumerate() {
            // A validator with a schedule override runs its *own* (wrong) view of
            // the committee history — the operator-error scenarios.
            let spec = schedule_overrides
                .iter()
                .find(|(overridden, _)| *overridden == index)
                .map(|(_, spec)| spec);
            let provider = match spec {
                None => SchemeProvider::new(
                    NAMESPACE.to_vec(),
                    schedule.clone(),
                    Some(cluster.keys[index].1.clone()),
                ),
                Some(spec) => cluster.provider_for(index, spec),
            };
            let mut validator = SimValidator {
                identity: identity.clone(),
                behavior: behaviors[index],
                env: env_factory(index, cluster.context.with_label("env")),
                activity: ActivityLog::new(),
                running: None,
                provider,
                // Stable across restarts: a restarted validator must find its own vote
                // journal (double-sign protection) and archives under the same prefix.
                partition_prefix: format!("{storage_prefix}-{index}"),
                incarnation: 0,
            };
            if !stopped.contains(&index) {
                Self::spawn(
                    &cluster.context,
                    &mut cluster.oracle,
                    index,
                    &mut validator,
                    &cluster.stack_tuner,
                )
                .await;
            }
            cluster.validators.push(validator);
        }
        cluster
    }

    /// Builds one validator's scheme provider over the given spec — used for
    /// schedule overrides at start and for fixing a validator's configuration
    /// between [`Self::crash`] and [`Self::restart`] (the config-fix-and-rejoin
    /// choreography).
    fn provider_for(&self, index: usize, spec: &ScheduleSpec) -> SchemeProvider {
        let schedule = Self::build_schedule(&self.keys, spec, self.keys.len());
        SchemeProvider::new(
            NAMESPACE.to_vec(),
            schedule,
            Some(self.keys[index].1.clone()),
        )
    }

    /// Replaces a stopped validator's schedule (its "deployed configuration") so
    /// the next [`Self::restart`] runs with the corrected committee history.
    pub fn reconfigure_schedule(&mut self, index: usize, spec: &ScheduleSpec) {
        assert!(
            self.validators[index].running.is_none(),
            "reconfigure a validator only while it is stopped (config changes are \
             deploy-and-restart operations)"
        );
        self.validators[index].provider = self.provider_for(index, spec);
    }

    /// Orphans a stopped validator's consensus scratch state (engine journals,
    /// marshal caches and archives) by moving it to a fresh storage prefix — the
    /// sim equivalent of the operator remedy "clear the consensus state directory
    /// and restart".
    ///
    /// Note the registered limitation: fresh consensus state over a *retained*
    /// chain (execution environment at height H > 0, era anchored at genesis) is
    /// not startable today — engines begin before backfill can supply the blocks
    /// that anchor their epochs, and the marshal's `Inline` panics on the missing
    /// starting-epoch block. Until "start consensus at a chain height without
    /// consensus history" exists (the EN-promotion shape), a rebuilt validator
    /// also rebuilds its chain: pair this with [`Self::reset_env`].
    pub fn clear_consensus_state(&mut self, index: usize) {
        assert!(
            self.validators[index].running.is_none(),
            "clear consensus state only while the validator is stopped"
        );
        let validator = &mut self.validators[index];
        validator.incarnation += 1;
        validator.partition_prefix = format!(
            "{}-reset-{}",
            validator.partition_prefix, validator.incarnation
        );
    }

    /// Replaces a stopped validator's execution environment — the "rebuilt from
    /// scratch" half of a full validator rebuild (fresh chain, resynced by
    /// consensus backfill from peers on the next start).
    pub fn reset_env(&mut self, index: usize, env: X) {
        assert!(
            self.validators[index].running.is_none(),
            "reset the environment only while the validator is stopped"
        );
        self.validators[index].env = env;
    }

    /// Builds the run's committee schedule from an index-based spec. An empty spec
    /// is the default single committee of every validator.
    fn build_schedule(
        keys: &[(ed25519::PrivateKey, group::Private)],
        spec: &ScheduleSpec,
        num_validators: usize,
    ) -> CommitteeSchedule {
        let committee_of = |indices: &[usize]| {
            commonware_utils::ordered::BiMap::try_from_iter(indices.iter().map(|&index| {
                let (network, bls) = &keys[index];
                (network.public_key(), compute_public::<MinPk>(bls))
            }))
            .expect("committee indices are distinct")
        };
        let entries: Vec<ScheduleEntry> = if spec.is_empty() {
            let everyone: Vec<usize> = (0..num_validators).collect();
            vec![ScheduleEntry {
                activation_epoch: 0,
                committee: committee_of(&everyone),
            }]
        } else {
            spec.iter()
                .map(|(activation_epoch, indices)| ScheduleEntry {
                    activation_epoch: *activation_epoch,
                    committee: committee_of(indices),
                })
                .collect()
        };
        CommitteeSchedule::new(entries).expect("scenario schedule is valid")
    }

    /// Registers the five p2p channels and starts whatever this validator's behavior
    /// calls for: the full stack, or a byzantine engine speaking raw consensus wire.
    async fn spawn(
        context: &deterministic::Context,
        oracle: &mut Oracle<PublicKey, deterministic::Context>,
        index: usize,
        validator: &mut SimValidator<X>,
        stack_tuner: &StackTuner,
    ) {
        let quota = Quota::per_second(NonZeroU32::MAX);
        let control = oracle.control(validator.identity.clone());
        let channels = Channels {
            votes: control.register(VOTES, quota).await.expect("register"),
            certificates: control
                .register(CERTIFICATES, quota)
                .await
                .expect("register"),
            certificate_backfill: control
                .register(CERTIFICATE_BACKFILL, quota)
                .await
                .expect("register"),
            block_broadcast: control
                .register(BLOCK_BROADCAST, quota)
                .await
                .expect("register"),
            block_backfill: control
                .register(BLOCK_BACKFILL, quota)
                .await
                .expect("register"),
        };

        validator.incarnation += 1;
        let incarnation = validator.incarnation;
        let label = format!("validator_{index}_run_{incarnation}");
        let running = match validator.behavior {
            Behavior::Honest => {
                let mut stack_config = StackConfig::new(validator.partition_prefix.clone());
                stack_tuner(&mut stack_config);
                let stack = start_validator(
                    context.with_label(&label),
                    stack_config,
                    validator.identity.clone(),
                    validator.provider.clone(),
                    validator.env.clone(),
                    oracle.control(validator.identity.clone()),
                    oracle.manager(),
                    channels,
                    Default::default(),
                    validator.activity.clone(),
                )
                .await;
                Running::Full(stack)
            }
            // The byzantine engines only speak the vote channel; the other channels sit
            // idle. They react to observed traffic with contradictory signed votes.
            // Honest validators mux their engine channels by epoch, so the attacker
            // must frame its traffic the same way to be heard at all — these
            // scenarios attack epoch 0, and the byzantine validator must be an
            // epoch-0 committee member for its signatures to count as evidence.
            Behavior::Conflicter => {
                let (votes, mux_task) =
                    Self::byzantine_votes_subchannel(context, &label, channels.votes).await;
                let engine = conflicter::Conflicter::<_, Scheme, Sha256>::new(
                    context.with_label(&label),
                    conflicter::Config {
                        scheme: validator
                            .provider
                            .scheme_for(Epoch::new(0))
                            .as_ref()
                            .clone(),
                    },
                );
                Running::Byzantine(vec![engine.start(votes), mux_task])
            }
            Behavior::Nuller => {
                let (votes, mux_task) =
                    Self::byzantine_votes_subchannel(context, &label, channels.votes).await;
                let engine = nuller::Nuller::<_, Scheme, Sha256>::new(
                    context.with_label(&label),
                    nuller::Config {
                        scheme: validator
                            .provider
                            .scheme_for(Epoch::new(0))
                            .as_ref()
                            .clone(),
                    },
                );
                Running::Byzantine(vec![engine.start(votes), mux_task])
            }
        };
        validator.running = Some(running);
    }

    /// Wraps a byzantine validator's votes channel in the same per-epoch mux honest
    /// engines use, registered for epoch 0 (the epoch these attacks target).
    async fn byzantine_votes_subchannel<TSender, TReceiver>(
        context: &deterministic::Context,
        label: &str,
        votes: (TSender, TReceiver),
    ) -> (
        (
            commonware_p2p::utils::mux::SubSender<TSender>,
            commonware_p2p::utils::mux::SubReceiver<TReceiver>,
        ),
        Handle<()>,
    )
    where
        TSender: commonware_p2p::Sender<PublicKey = PublicKey>,
        TReceiver: commonware_p2p::Receiver<PublicKey = PublicKey>,
    {
        let (muxer, mut mux) = commonware_p2p::utils::mux::Muxer::new(
            context.with_label(&format!("{label}_votes_mux")),
            votes.0,
            votes.1,
            1024,
        );
        let mux_task = context
            .with_label(&format!("{label}_votes_mux_task"))
            .spawn(|_| async move {
                let _ = muxer.run().await;
            });
        let sub = mux
            .register(0)
            .await
            .expect("register byzantine subchannel");
        (sub, mux_task)
    }

    /// Indices of the honest validators (assertions about committed chains and health
    /// only ever apply to these).
    pub fn honest_indices(&self) -> Vec<usize> {
        self.validators
            .iter()
            .enumerate()
            .filter(|(_, validator)| validator.behavior == Behavior::Honest)
            .map(|(index, _)| index)
            .collect()
    }

    /// Stops a validator (abrupt, like a crash — no clean shutdown).
    pub fn crash(&mut self, index: usize) {
        match self.validators[index].running.take() {
            Some(Running::Full(stack)) => stack.abort(),
            Some(Running::Byzantine(handles)) => {
                for handle in handles {
                    handle.abort();
                }
            }
            None => {}
        }
    }

    /// Starts a crashed validator again over its surviving storage. Its vote journal
    /// replays (so it cannot double-sign) and it catches up via gossip and backfill.
    /// Also brings up a validator that was provisioned stopped and has no storage yet
    /// (see [`Self::start_with_env_stopped`]) — a first start and a restart differ
    /// only in what the storage holds.
    pub async fn restart(&mut self, index: usize) {
        assert!(
            self.validators[index].running.is_none(),
            "validator {index} is already running"
        );
        Self::spawn(
            &self.context,
            &mut self.oracle,
            index,
            &mut self.validators[index],
            &self.stack_tuner,
        )
        .await;
    }

    /// Links every pair of validators symmetrically with the given quality.
    async fn link_full_mesh_between(&mut self, identities: &[PublicKey], link: Link) {
        for a in identities {
            for b in identities {
                if a != b {
                    self.oracle
                        .add_link(a.clone(), b.clone(), link.clone())
                        .await
                        .expect("linking validators failed");
                }
            }
        }
    }

    /// Cuts the network into isolated groups: links *between* groups are removed, links
    /// within each group stay. Groups must cover all validators you care about; indices
    /// not listed anywhere keep their existing links untouched.
    pub async fn partition(&mut self, groups: &[&[usize]]) {
        for (group_position, group) in groups.iter().enumerate() {
            for other_group in groups.iter().skip(group_position + 1) {
                for &a in group.iter() {
                    for &b in other_group.iter() {
                        let a = self.validators[a].identity.clone();
                        let b = self.validators[b].identity.clone();
                        self.oracle
                            .remove_link(a.clone(), b.clone())
                            .await
                            .expect("removing link");
                        self.oracle.remove_link(b, a).await.expect("removing link");
                    }
                }
            }
        }
    }

    /// Restores a symmetric full mesh with the given link quality (heals any partition
    /// and overrides any per-link degradation).
    pub async fn heal(&mut self, link: Link) {
        let identities: Vec<PublicKey> = self
            .validators
            .iter()
            .map(|validator| validator.identity.clone())
            .collect();
        for a in &identities {
            for b in &identities {
                if a != b {
                    // A link may or may not currently exist depending on what the
                    // scenario did before; reset it unconditionally.
                    let _ = self.oracle.remove_link(a.clone(), b.clone()).await;
                    self.oracle
                        .add_link(a.clone(), b.clone(), link.clone())
                        .await
                        .expect("re-linking validators failed");
                }
            }
        }
    }

    /// Lets in-flight deliveries drain (virtual time). Use after reshaping the network
    /// and before asserting stillness: finality certificates assembled just before a
    /// partition may legitimately reach a validator just after it.
    pub async fn settle(&self, duration: Duration) {
        self.context.sleep(duration).await;
    }

    /// Waits (in virtual time) until every listed validator committed at least `height`.
    pub async fn wait_for_committed_height(&self, indices: &[usize], height: u64) {
        loop {
            let reached = indices.iter().all(|&index| {
                self.validators[index]
                    .env
                    .committed_tip()
                    .is_some_and(|tip| tip >= height)
            });
            if reached {
                return;
            }
            self.context.sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits until all *honest* validators committed at least `height`.
    pub async fn wait_for_committed_height_all(&self, height: u64) {
        self.wait_for_committed_height(&self.honest_indices(), height)
            .await;
    }

    /// Asserts that no listed validator commits anything new for `duration` of virtual
    /// time. This is how partition tests check safety: with no quorum, the chain must
    /// stand still rather than fork.
    pub async fn assert_no_progress_for(&self, indices: &[usize], duration: Duration) {
        let tips_before: Vec<Option<u64>> = indices
            .iter()
            .map(|&index| self.validators[index].env.committed_tip())
            .collect();
        self.context.sleep(duration).await;
        for (position, &index) in indices.iter().enumerate() {
            let tip_after = self.validators[index].env.committed_tip();
            assert_eq!(
                tips_before[position], tip_after,
                "validator {index} made progress during a period that must be quiet",
            );
        }
    }

    /// The committed height of one validator (0 when it has committed nothing).
    pub fn committed_height(&self, index: usize) -> u64 {
        self.validators[index].env.committed_tip().unwrap_or(0)
    }

    /// The agreement property: every honest validator's committed chain (as a digest
    /// sequence) is a prefix of the longest one, and every honest validator reached at
    /// least `minimum_height`.
    pub fn assert_committed_chains_agree(&self, minimum_height: u64) {
        self.assert_committed_chains_agree_between(&self.honest_indices(), minimum_height);
    }

    /// [`Self::assert_committed_chains_agree`] over a chosen subset — for
    /// reconfiguration scenarios, where validators scheduled out of the committee
    /// (or still catching up) legitimately trail the members.
    pub fn assert_committed_chains_agree_between(&self, indices: &[usize], minimum_height: u64) {
        let chains: Vec<(usize, Vec<Sha256Digest>)> = indices
            .iter()
            .map(|&index| (index, self.validators[index].env.committed_chain_digests()))
            .collect();
        for (index, chain) in &chains {
            assert!(
                chain.len() as u64 >= minimum_height,
                "validator {index} committed only {} blocks, expected at least {minimum_height}",
                chain.len(),
            );
        }
        let longest = &chains
            .iter()
            .max_by_key(|(_, chain)| chain.len())
            .expect("at least one validator in the agreement set")
            .1;
        for (index, chain) in &chains {
            for (height_index, digest) in chain.iter().enumerate() {
                assert_eq!(
                    digest,
                    &longest[height_index],
                    "validator {index} committed a different block at height {}",
                    height_index + 1,
                );
            }
        }
    }

    /// No honest validator observed any Byzantine fault evidence.
    pub fn assert_no_faults(&self) {
        for &index in &self.honest_indices() {
            assert_eq!(
                self.validators[index].activity.faults(),
                0,
                "validator {index} observed fault evidence"
            );
        }
    }

    /// Every honest validator observed fault evidence, and all of it points at exactly
    /// the given committee positions — nobody else got incriminated.
    pub fn assert_faults_point_exactly_at(&self, culprit_indices: &[usize]) {
        let expected: std::collections::BTreeSet<u32> =
            culprit_indices.iter().map(|&index| index as u32).collect();
        for &index in &self.honest_indices() {
            let culprits = self.validators[index].activity.fault_culprits();
            assert!(
                self.validators[index].activity.faults() > 0,
                "validator {index} observed no fault evidence at all",
            );
            assert_eq!(
                culprits, expected,
                "validator {index} recorded fault evidence for an unexpected set of validators",
            );
        }
    }

    /// The network oracle must not have banned anyone (bans mean invalid signatures or
    /// protocol violations were detected on the wire).
    pub async fn assert_no_blocked_peers(&mut self) {
        let blocked = self.oracle.blocked().await.expect("oracle blocked query");
        assert!(blocked.is_empty(), "peers were blocked: {blocked:?}");
    }

    /// Honest validators must have blocked the given byzantine validator — and no honest
    /// validator may have been blocked by anyone.
    pub async fn assert_blocked_only(&mut self, byzantine_index: usize) {
        let byzantine = self.validators[byzantine_index].identity.clone();
        let blocked = self.oracle.blocked().await.expect("oracle blocked query");
        assert!(
            !blocked.is_empty(),
            "expected the byzantine validator to get blocked by honest peers",
        );
        for (blocker, blocked_peer) in blocked {
            assert_ne!(
                blocker, byzantine,
                "the byzantine validator should not be the one doing the blocking",
            );
            assert_eq!(
                blocked_peer, byzantine,
                "an honest validator was blocked; only the byzantine one may be",
            );
        }
    }
}
