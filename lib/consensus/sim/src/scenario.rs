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
pub fn run_scenario<F, Fut>(
    name: &str,
    seeds: impl IntoIterator<Item = u64>,
    virtual_timeout: Duration,
    body: F,
) where
    F: Fn(deterministic::Context) -> Fut,
    Fut: Future<Output = ()>,
{
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
