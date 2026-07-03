//! A simulated validator cluster: N full consensus stacks over an in-memory network,
//! inside the deterministic runtime.
//!
//! Everything a test needs in one handle: start a cluster, shape the network, crash and
//! restart validators, and assert on what was committed. Time is virtual (a minute of
//! consensus takes milliseconds of wall clock) and every run is reproducible from the
//! runtime seed.

use crate::activity::ActivityLog;
use crate::block::SimBlock;
use crate::execution::MockExecution;
use commonware_consensus::simplex::scheme::bls12381_multisig;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::certificate::mocks::Fixture;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::simulated::{Config as NetworkConfig, Link, Network, Oracle};
use commonware_runtime::{Clock, Metrics, Quota, deterministic};
use commonware_utils::NZUsize;
use std::num::NonZeroU32;
use std::time::Duration;
use zksync_os_consensus_core::types::Scheme;
use zksync_os_consensus_core::{Channels, StackConfig, ValidatorStack, start_validator};

/// Domain-separation namespace for all consensus signatures in simulation.
const NAMESPACE: &[u8] = b"zksync-os-consensus-sim";

/// Channel ids on the simulated network, one per consensus traffic class.
const VOTES: u64 = 0;
const CERTIFICATES: u64 = 1;
const CERTIFICATE_BACKFILL: u64 = 2;
const BLOCK_BROADCAST: u64 = 3;
const BLOCK_BACKFILL: u64 = 4;

pub struct SimCluster {
    context: deterministic::Context,
    pub oracle: Oracle<PublicKey, deterministic::Context>,
    pub validators: Vec<SimValidator>,
}

pub struct SimValidator {
    pub identity: PublicKey,
    /// This validator's view of the chain; survives crash/restart like a node's disk.
    pub env: MockExecution,
    /// Records fault evidence this validator observed.
    pub activity: ActivityLog,
    stack: Option<ValidatorStack<SimBlock>>,
    scheme: Scheme,
    partition_prefix: String,
    /// How many times this validator has been started. Only used to give each
    /// incarnation a distinct metrics namespace: in production a restart is a fresh
    /// process with a fresh metrics registry, but in simulation the registry lives as
    /// long as the run, and registering the same metric twice is an error.
    incarnation: usize,
}

impl SimCluster {
    /// Starts `num_validators` fully-linked validators with the given link quality.
    pub async fn start(
        mut context: deterministic::Context,
        num_validators: u32,
        link: Link,
    ) -> Self {
        let Fixture {
            participants,
            schemes,
            ..
        } = bls12381_multisig::fixture::<MinPk, _>(&mut context, NAMESPACE, num_validators);

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

        // Symmetric full mesh; tests reshape links through `oracle` afterwards.
        let oracle = oracle;
        for a in &participants {
            for b in &participants {
                if a != b {
                    oracle
                        .add_link(a.clone(), b.clone(), link.clone())
                        .await
                        .expect("linking validators failed");
                }
            }
        }

        let mut cluster = Self {
            context,
            oracle,
            validators: Vec::new(),
        };
        for (index, identity) in participants.iter().enumerate() {
            let mut validator = SimValidator {
                identity: identity.clone(),
                env: MockExecution::new(),
                activity: ActivityLog::new(),
                stack: None,
                scheme: schemes[index].clone(),
                // Stable across restarts: a restarted validator must find its own vote
                // journal (double-sign protection) and archives under the same prefix.
                partition_prefix: format!("validator-{index}"),
                incarnation: 0,
            };
            Self::spawn_stack(&cluster.context, &mut cluster.oracle, index, &mut validator).await;
            cluster.validators.push(validator);
        }
        cluster
    }

    /// Registers the five p2p channels and starts the full stack for one validator.
    async fn spawn_stack(
        context: &deterministic::Context,
        oracle: &mut Oracle<PublicKey, deterministic::Context>,
        index: usize,
        validator: &mut SimValidator,
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
        let stack = start_validator(
            context.with_label(&format!("validator_{index}_run_{incarnation}")),
            StackConfig::new(validator.partition_prefix.clone()),
            validator.identity.clone(),
            validator.scheme.clone(),
            validator.env.clone(),
            oracle.control(validator.identity.clone()),
            oracle.manager(),
            channels,
            (),
            validator.activity.clone(),
        )
        .await;
        validator.stack = Some(stack);
    }

    /// Stops a validator (abrupt, like a crash — no clean shutdown).
    pub fn crash(&mut self, index: usize) {
        if let Some(stack) = self.validators[index].stack.take() {
            stack.abort();
        }
    }

    /// Starts a crashed validator again over its surviving storage. Its vote journal
    /// replays (so it cannot double-sign) and it catches up via gossip and backfill.
    pub async fn restart(&mut self, index: usize) {
        assert!(
            self.validators[index].stack.is_none(),
            "validator {index} is already running"
        );
        Self::spawn_stack(
            &self.context,
            &mut self.oracle,
            index,
            &mut self.validators[index],
        )
        .await;
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

    /// Waits until *all* validators committed at least `height`.
    pub async fn wait_for_committed_height_all(&self, height: u64) {
        let all: Vec<usize> = (0..self.validators.len()).collect();
        self.wait_for_committed_height(&all, height).await;
    }

    /// The agreement property: every validator's committed chain is a prefix of the
    /// longest one (identical blocks at every common height), and everyone reached at
    /// least `minimum_height`.
    pub fn assert_committed_chains_agree(&self, minimum_height: u64) {
        let chains: Vec<Vec<SimBlock>> = self
            .validators
            .iter()
            .map(|validator| validator.env.committed_chain())
            .collect();
        for (index, chain) in chains.iter().enumerate() {
            assert!(
                chain.len() as u64 >= minimum_height,
                "validator {index} committed only {} blocks, expected at least {minimum_height}",
                chain.len(),
            );
        }
        let longest = chains
            .iter()
            .max_by_key(|chain| chain.len())
            .expect("at least one validator");
        for (index, chain) in chains.iter().enumerate() {
            for (height_index, block) in chain.iter().enumerate() {
                assert_eq!(
                    block,
                    &longest[height_index],
                    "validator {index} committed a different block at height {}",
                    height_index + 1,
                );
            }
        }
    }

    /// No honest validator may ever observe Byzantine fault evidence in these tests.
    pub fn assert_no_faults(&self) {
        for (index, validator) in self.validators.iter().enumerate() {
            assert_eq!(
                validator.activity.faults(),
                0,
                "validator {index} observed fault evidence"
            );
        }
    }

    /// The network oracle must not have banned anyone (bans mean invalid signatures or
    /// protocol violations were detected on the wire).
    pub async fn assert_no_blocked_peers(&mut self) {
        let blocked = self.oracle.blocked().await.expect("oracle blocked query");
        assert!(blocked.is_empty(), "peers were blocked: {blocked:?}");
    }
}
