//! Runs a scenario across seeds, proving determinism along the way.
//!
//! Every scenario in the suite goes through [`run_scenario`], which gives each one the
//! same guarantees for free:
//!
//! - **Seed sweeping**: the same scenario logic runs under several scheduler seeds, each
//!   a genuinely different interleaving of messages, timers, and task wakeups. A bug that
//!   only bites under one interleaving still has a deterministic reproducer: the seed.
//! - **Determinism regression**: every seed is executed *twice* and the runtime's
//!   auditor fingerprints (a rolling hash over every scheduling decision, RNG draw, and
//!   network event) must match bit-for-bit. If someone accidentally introduces a source
//!   of nondeterminism — wall-clock time, a global RNG, iteration over an unordered map
//!   that feeds scheduling — the whole suite fails loudly rather than becoming flaky.
//!
//! A failing scenario prints its seed; rerunning that one seed reproduces the failure
//! exactly, down to every message on the simulated wire.

use commonware_runtime::{Runner, deterministic};
use std::future::Future;
use std::time::Duration;

/// Executes `body` twice for every seed and asserts bit-exact reproduction.
///
/// `virtual_timeout` bounds the scenario in *virtual* time: a scenario that fails to
/// finish within it panics ("runtime timeout"), turning liveness bugs into loud failures
/// instead of hangs. Generous values cost nothing — virtual time is free.
///
/// Seed-sweep override: when the `CONSENSUS_SIM_SEEDS` environment variable is set to
/// a number N, every scenario sweeps seeds `0..N` instead of the set it was called
/// with. PR CI leaves it unset (each scenario's own small set keeps the suite fast);
/// the nightly lane sets it large to hunt interleavings no small sweep would hit. A
/// failure still names the exact seed, so the reproducer is one local run.
pub fn run_scenario<F, Fut>(
    name: &str,
    seeds: impl IntoIterator<Item = u64>,
    virtual_timeout: Duration,
    body: F,
) where
    F: Fn(deterministic::Context) -> Fut,
    Fut: Future<Output = ()>,
{
    // Debugging aid: `CONSENSUS_SIM_LOG=debug cargo test ... -- --nocapture` gets
    // component-level logs out of a failing scenario. Logging writes to stderr
    // outside the simulated runtime, so it cannot affect determinism (and the
    // double-run below keeps proving that).
    if let Ok(filter) = std::env::var("CONSENSUS_SIM_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .without_time()
            .try_init();
    }
    let seeds: Vec<u64> = match std::env::var("CONSENSUS_SIM_SEEDS") {
        Ok(count) => {
            let count: u64 = count
                .parse()
                .expect("CONSENSUS_SIM_SEEDS must be a number (the seed-sweep size)");
            (0..count).collect()
        }
        Err(_) => seeds.into_iter().collect(),
    };
    for seed in seeds {
        let first = fingerprint(seed, virtual_timeout, &body);
        let second = fingerprint(seed, virtual_timeout, &body);
        assert_eq!(
            first, second,
            "scenario `{name}` with seed {seed} did not reproduce bit-exactly — \
             a source of nondeterminism crept in",
        );
    }
}

/// Runs `body` once under the given seed and returns the runtime's execution fingerprint.
/// Public so the suite can sanity-check that fingerprints of *different* executions
/// actually differ (i.e. the fingerprint really captures the execution).
pub fn fingerprint<F, Fut>(seed: u64, virtual_timeout: Duration, body: &F) -> String
where
    F: Fn(deterministic::Context) -> Fut,
    Fut: Future<Output = ()>,
{
    let runner = deterministic::Runner::new(
        deterministic::Config::new()
            .with_seed(seed)
            .with_timeout(Some(virtual_timeout)),
    );
    runner.start(|context| async move {
        let auditor = context.auditor().clone();
        body(context).await;
        auditor.state()
    })
}
