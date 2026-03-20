# Pipeline Health Monitor Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the global `ComponentStateReporter` singleton with a watch-channel-based `ComponentHealthReporter` and introduce a `PipelineHealthMonitor` that evaluates per-component backpressure conditions and signals `TransactionAcceptanceState` to the RPC layer.

**Architecture:** Each pipeline component holds a `ComponentHealthReporter` whose `watch::Receiver<ComponentHealth>` is registered with `PipelineHealthMonitor` at wiring time. The monitor evaluates time-in-`WaitingSend` and block-lag conditions on a configurable interval and is the sole writer of the `TransactionAcceptanceState` channel consumed by `TxHandler`. The `/status/health` endpoint is extended to return the full pipeline snapshot.

**Tech Stack:** Rust, tokio (`watch`, `time`), axum, vise (Prometheus metrics), serde/serde_json, cargo nextest

---

## File Map

### New files
- `lib/pipeline_health/Cargo.toml` — new crate
- `lib/pipeline_health/src/lib.rs` — exports: `ComponentId`, `BackpressureCondition`, `PipelineHealthConfig`, `PipelineHealthMonitor`
- `lib/pipeline_health/src/monitor.rs` — `PipelineHealthMonitor::run`, `evaluate_and_update`, `evaluate`, `emit_metrics`
- `lib/pipeline_health/src/config.rs` — `ComponentId`, `BackpressureCondition`, `PipelineHealthConfig`
- `lib/pipeline_health/src/metrics.rs` — `MONITOR_METRICS` Prometheus struct

### Modified files
- `lib/types/src/transaction_acceptance_state.rs` — add `PipelineBackpressure` variant, `BackpressureCause`, `BackpressureTrigger`
- `lib/observability/src/lib.rs` — add `ComponentHealthReporter` export, keep `ComponentStateReporter` temporarily
- `lib/observability/src/component_health_reporter.rs` — new file: `ComponentHealthReporter`, `ComponentHealth`
- `lib/observability/Cargo.toml` — no changes needed (tokio watch already available)
- `lib/status/src/lib.rs` — extend `AppState`, extend `run_status_server` signature
- `lib/status/src/health.rs` — full pipeline snapshot handler
- `lib/status/Cargo.toml` — add `pipeline_health` and `types` deps
- `node/bin/Cargo.toml` — add `pipeline_health` dep
- `node/bin/src/lib.rs` — create monitor, register all components, pass acceptance receiver to status server and TxHandler
- `lib/rpc/src/tx_handler.rs` — remove second acceptance check (BackpressureHandle); already has `acceptance_state` field
- `lib/sequencer/src/execution/block_executor.rs` — replace `ComponentStateReporter` with `ComponentHealthReporter`
- `lib/sequencer/src/canonizer.rs` (or similar) — replace `ComponentStateReporter`
- `node/bin/src/block_applier.rs` (or similar) — replace `ComponentStateReporter`
- `node/bin/src/tree_manager.rs` — replace `ComponentStateReporter`
- `node/bin/src/batcher/mod.rs` — replace `ComponentStateReporter`
- `node/bin/src/prover_input_generator/mod.rs` — replace `ComponentStateReporter`
- `node/bin/src/prover_api/fri_job_manager.rs` — replace `ComponentStateReporter`
- `node/bin/src/prover_api/snark_job_manager.rs` — replace `ComponentStateReporter`
- `node/bin/src/prover_api/gapless_committer.rs` — replace `ComponentStateReporter`
- `node/bin/src/prover_api/gapless_l1_proof_sender.rs` — replace `ComponentStateReporter`
- `lib/l1_sender/src/lib.rs` — replace `ComponentStateReporter`
- `lib/l1_sender/src/upgrade_gatekeeper.rs` — replace `ComponentStateReporter`
- `lib/batch_verification/src/client/mod.rs` — replace `ComponentStateReporter`
- `lib/batch_verification/src/sequencer/component.rs` — replace `ComponentStateReporter`
- `lib/priority_tree/src/lib.rs` — replace `ComponentStateReporter`
- `lib/revm_consistency_checker/src/node.rs` — replace `ComponentStateReporter` (see RevmConsistencyChecker note below)
- `zksync_os_integration_tests/` — new integration test

---

## Chunk 1: Core Types

### Task 1: Extend `TransactionAcceptanceState`

**Files:**
- Modify: `lib/types/src/transaction_acceptance_state.rs`

- [ ] **Step 1: Read the current file**

```bash
cat lib/types/src/transaction_acceptance_state.rs
```

- [ ] **Step 2: Write failing unit test first**

Add a test module at the bottom of the file verifying the new variants compile and serialize correctly:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pipeline_backpressure_not_accepting() {
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockLagTooHigh {
                threshold: 500,
                actual: 782,
            },
        };
        let state = TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause.clone()],
            },
        );
        assert!(matches!(
            state,
            TransactionAcceptanceState::NotAccepting(
                NotAcceptingReason::PipelineBackpressure { .. }
            )
        ));
        assert_eq!(cause.component, "fri_job_manager");
    }

    #[test]
    fn waiting_send_too_long_trigger() {
        let trigger = BackpressureTrigger::WaitingSendTooLong {
            threshold: Duration::from_secs(3600),
            actual: Duration::from_secs(4215),
        };
        assert!(matches!(trigger, BackpressureTrigger::WaitingSendTooLong { .. }));
    }
}
```

Run: `cargo nextest run -p zksync_os_types -- transaction_acceptance_state -v`
Expected: FAIL — types don't exist yet

- [ ] **Step 3: Add new types**

Replace the content of `lib/types/src/transaction_acceptance_state.rs`:

```rust
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum TransactionAcceptanceState {
    Accepting,
    NotAccepting(NotAcceptingReason),
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotAcceptingReason {
    BlockProductionDisabled,
    PipelineBackpressure { causes: Vec<BackpressureCause> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackpressureCause {
    pub component: &'static str,
    pub trigger: BackpressureTrigger,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackpressureTrigger {
    WaitingSendTooLong { threshold: Duration, actual: Duration },
    BlockLagTooHigh    { threshold: u64,      actual: u64 },
}
```

- [ ] **Step 4: Fix any compilation errors from `NotAcceptingReason::BlockProductionDisabled` usage**

The existing code that matches on `NotAcceptingReason::BlockProductionDisabled` uses `*reason` (dereferencing a `Copy` type). After this change it's no longer `Copy`. Search and fix:

```bash
grep -rn "NotAcceptingReason\|BlockProductionDisabled\|NotAcceptingTransactions" \
  --include="*.rs" lib/ node/ zksync_os_integration_tests/
```

For each match referencing `reason` as Copy, change `*reason` to `reason.clone()`.

- [ ] **Step 5: Run the test**

Run: `cargo nextest run -p zksync_os_types -- transaction_acceptance_state -v`
Expected: PASS

- [ ] **Step 6: Run full unit tests to check for regressions**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add lib/types/src/transaction_acceptance_state.rs
git commit -m "feat(types): add PipelineBackpressure variant and BackpressureCause types"
```

---

### Task 2: Add `ComponentHealth` and `ComponentHealthReporter`

**Files:**
- Create: `lib/observability/src/component_health_reporter.rs`
- Modify: `lib/observability/src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `lib/observability/src/component_health_reporter.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::GenericComponentState;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn reporter_new_starts_in_waiting_recv() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::WaitingRecv);
        assert_eq!(health.last_processed_seq, 0);
        drop(reporter);
    }

    #[tokio::test]
    async fn enter_state_updates_receiver() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.enter_state(GenericComponentState::Processing);
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::Processing);
    }

    #[tokio::test]
    async fn record_processed_updates_seq() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.record_processed(42);
        assert_eq!(rx.borrow().last_processed_seq, 42);
        reporter.record_processed(100);
        assert_eq!(rx.borrow().last_processed_seq, 100);
    }

    #[tokio::test]
    async fn state_entered_at_updates_on_enter_state() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let t0 = rx.borrow().state_entered_at;
        sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Processing);
        let t1 = rx.borrow().state_entered_at;
        assert!(t1 > t0, "state_entered_at must advance");
    }

    #[tokio::test]
    async fn multiple_reporters_independent() {
        let (r1, rx1) = ComponentHealthReporter::new("c1");
        let (r2, rx2) = ComponentHealthReporter::new("c2");
        r1.record_processed(10);
        r2.record_processed(20);
        assert_eq!(rx1.borrow().last_processed_seq, 10);
        assert_eq!(rx2.borrow().last_processed_seq, 20);
    }
}
```

Run: `cargo nextest run -p zksync_os_observability -- component_health -v`
Expected: FAIL — types not defined

- [ ] **Step 2: Implement `ComponentHealth` and `ComponentHealthReporter`**

Complete the file:

```rust
use crate::generic_component_state::GenericComponentState;
use tokio::{sync::watch, time::Instant};

/// Health snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentHealth {
    pub state: GenericComponentState,
    /// When the current state was entered (monotonic).
    pub state_entered_at: Instant,
    /// Block number of the last item successfully sent downstream.
    /// For batch-level components: highest block in the last processed batch.
    pub last_processed_seq: u64,
}

/// Replaces `ComponentStateReporter`.
/// Uses `watch::Sender` — updates are infallible, no background task, no global state.
pub struct ComponentHealthReporter {
    sender: watch::Sender<ComponentHealth>,
    component: &'static str,
}

impl ComponentHealthReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    /// No global state — safe to construct multiple instances in tests.
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentHealth>) {
        let initial = ComponentHealth {
            state: GenericComponentState::WaitingRecv,
            state_entered_at: Instant::now(),
            last_processed_seq: 0,
        };
        let (sender, receiver) = watch::channel(initial);
        (Self { sender, component }, receiver)
    }

    /// Transition to a new state and record time-in-previous-state metric inline.
    pub fn enter_state(&self, new_state: GenericComponentState) {
        let now = Instant::now();
        self.sender.send_modify(|health| {
            let elapsed = now.duration_since(health.state_entered_at);
            // Emit the existing time_in_state metric — same metric as ComponentStateReporter,
            // preserving dashboard compatibility.
            //
            // ⚠️  BEFORE WRITING THIS CODE: read `lib/observability/src/component_state_reporter.rs`
            // to find the exact metric struct name, field name, key type, and accumulation method.
            // Common patterns in this codebase:
            //   - GENERAL_METRICS.time_in_state[&(component, state)].inc_by(secs)   (Counter<f64>)
            //   - COMPONENT_METRICS.time_in_state[&(component, state)].observe(elapsed) (Histogram)
            // Use whichever matches the existing reporter. Wrong method → compile error.
            crate::metrics::COMPONENT_METRICS.time_in_state
                [&(self.component, health.state)]
                .observe(elapsed);
            health.state = new_state;
            health.state_entered_at = now;
        });
    }

    /// Record the block number of the last item successfully sent downstream.
    /// Call this after `output.send(result).await` returns successfully.
    pub fn record_processed(&self, block_seq: u64) {
        self.sender.send_modify(|health| {
            health.last_processed_seq = block_seq;
        });
    }
}
```

> **Note:** The `crate::metrics::COMPONENT_METRICS.time_in_state` reference matches the existing metric in `component_state_reporter.rs`. Check the exact field name by reading that file before finalizing — it may be `ComponentStateMetrics` or similar.

- [ ] **Step 3: Export from `lib/observability/src/lib.rs`**

Add to the existing exports:
```rust
pub use component_health_reporter::{ComponentHealth, ComponentHealthReporter};
```

Keep `ComponentStateReporter` export in place for now (removed in Task 11).

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p zksync_os_observability -- component_health -v`
Expected: PASS

- [ ] **Step 5: Check full workspace still compiles**

Run: `cargo build --workspace 2>&1 | head -30`
Expected: Compiles without errors

- [ ] **Step 6: Commit**

```bash
git add lib/observability/src/component_health_reporter.rs lib/observability/src/lib.rs
git commit -m "feat(observability): add ComponentHealthReporter and ComponentHealth"
```

---

## Chunk 2: PipelineHealthMonitor Crate

### Task 3: Create `lib/pipeline_health` crate with config types

**Files:**
- Create: `lib/pipeline_health/Cargo.toml`
- Create: `lib/pipeline_health/src/lib.rs`
- Create: `lib/pipeline_health/src/config.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "zksync_os_pipeline_health"
version = "0.1.0"
edition = "2021"

[dependencies]
zksync_os_observability = { path = "../observability" }
zksync_os_types         = { path = "../types" }
tokio                   = { workspace = true, features = ["time", "sync", "macros"] }
vise                    = { workspace = true }
tracing                 = { workspace = true }

[dev-dependencies]
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: Write failing tests for config types**

Create `lib/pipeline_health/src/config.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn default_config_has_one_second_interval() {
        let config = PipelineHealthConfig::default();
        assert_eq!(config.eval_interval, Duration::from_secs(1));
    }

    #[test]
    fn default_conditions_are_all_none() {
        let config = PipelineHealthConfig::default();
        let cond = config.condition_for(ComponentId::BlockExecutor);
        assert!(cond.max_waiting_send_duration.is_none());
        assert!(cond.max_block_lag.is_none());
    }

    #[test]
    fn condition_for_all_variants() {
        let config = PipelineHealthConfig::default();
        // Smoke-test that all ComponentId variants are handled without panic.
        use ComponentId::*;
        for id in [BlockExecutor, BlockApplier, TreeManager, BlockCanonizer,
                   ProverInputGenerator, Batcher, BatchVerification, FriJobManager,
                   GaplessCommitter, UpgradeGatekeeper, L1SenderCommit, SnarkJobManager,
                   GaplessL1ProofSender, L1SenderProve, PriorityTree, L1SenderExecute] {
            let _ = config.condition_for(id);
        }
    }

    #[test]
    fn fri_job_manager_is_reactive_others_are_not() {
        assert!(ComponentId::FriJobManager.is_reactive());
        assert!(!ComponentId::BlockExecutor.is_reactive());
        assert!(!ComponentId::SnarkJobManager.is_reactive());
        assert!(!ComponentId::L1SenderCommit.is_reactive());
    }

    #[test]
    fn as_str_returns_snake_case() {
        assert_eq!(ComponentId::BlockExecutor.as_str(), "block_executor");
        assert_eq!(ComponentId::FriJobManager.as_str(), "fri_job_manager");
        assert_eq!(ComponentId::GaplessL1ProofSender.as_str(), "gapless_l1_proof_sender");
    }
}
```

Run: `cargo nextest run -p zksync_os_pipeline_health -- config -v`
Expected: FAIL — types not defined

- [ ] **Step 3: Implement config types**

Complete `lib/pipeline_health/src/config.rs`:

```rust
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComponentId {
    // Both pipelines
    BlockExecutor,
    BlockApplier,
    TreeManager,
    // Main node — consensus
    BlockCanonizer,
    // Main node — proving and settlement
    ProverInputGenerator,
    Batcher,
    BatchVerification,
    FriJobManager,
    GaplessCommitter,
    UpgradeGatekeeper,
    L1SenderCommit,
    SnarkJobManager,
    GaplessL1ProofSender,
    L1SenderProve,
    PriorityTree,
    L1SenderExecute,
}

impl ComponentId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlockExecutor        => "block_executor",
            Self::BlockApplier         => "block_applier",
            Self::TreeManager          => "tree_manager",
            Self::BlockCanonizer       => "block_canonizer",
            Self::ProverInputGenerator => "prover_input_generator",
            Self::Batcher              => "batcher",
            Self::BatchVerification    => "batch_verification",
            Self::FriJobManager        => "fri_job_manager",
            Self::GaplessCommitter     => "gapless_committer",
            Self::UpgradeGatekeeper    => "upgrade_gatekeeper",
            Self::L1SenderCommit       => "l1_sender_commit",
            Self::SnarkJobManager      => "snark_job_manager",
            Self::GaplessL1ProofSender => "gapless_l1_proof_sender",
            Self::L1SenderProve        => "l1_sender_prove",
            Self::PriorityTree         => "priority_tree",
            Self::L1SenderExecute      => "l1_sender_execute",
        }
    }

    /// Whether this component is reactive (never holds WaitingSend for measurable duration).
    /// Only `FriJobManager` uses `try_reserve` instead of blocking `.await` in WaitingSend.
    /// SnarkJobManager is NOT reactive — it blocks in `.await` in WaitingSend.
    pub fn is_reactive(self) -> bool {
        matches!(self, Self::FriJobManager)
    }
}

#[derive(Default, Clone, Debug)]
pub struct BackpressureCondition {
    /// Trigger if component stays in WaitingSend longer than this duration.
    /// Ignored for reactive components (FriJobManager).
    pub max_waiting_send_duration: Option<Duration>,
    /// Trigger if component is more than N blocks behind the pipeline head.
    /// For pipeline-loop components: only evaluated when in WaitingSend.
    /// For reactive components: evaluated regardless of state.
    pub max_block_lag: Option<u64>,
}

pub struct PipelineHealthConfig {
    /// How often the monitor re-evaluates all conditions. Must not be zero.
    pub eval_interval: Duration,
    // Both pipelines
    pub block_executor:           BackpressureCondition,
    pub block_applier:            BackpressureCondition,
    pub tree_manager:             BackpressureCondition,
    // Main node — consensus
    pub block_canonizer:          BackpressureCondition,
    // Main node — proving and settlement
    pub prover_input_generator:   BackpressureCondition,
    pub batcher:                  BackpressureCondition,
    pub batch_verification:       BackpressureCondition,
    pub fri_job_manager:          BackpressureCondition,
    pub gapless_committer:        BackpressureCondition,
    pub upgrade_gatekeeper:       BackpressureCondition,
    pub l1_sender_commit:         BackpressureCondition,
    pub snark_job_manager:        BackpressureCondition,
    pub gapless_l1_proof_sender:  BackpressureCondition,
    pub l1_sender_prove:          BackpressureCondition,
    pub priority_tree:            BackpressureCondition,
    pub l1_sender_execute:        BackpressureCondition,
}

impl Default for PipelineHealthConfig {
    fn default() -> Self {
        Self {
            eval_interval:          Duration::from_secs(1),
            block_executor:         BackpressureCondition::default(),
            block_applier:          BackpressureCondition::default(),
            tree_manager:           BackpressureCondition::default(),
            block_canonizer:        BackpressureCondition::default(),
            prover_input_generator: BackpressureCondition::default(),
            batcher:                BackpressureCondition::default(),
            batch_verification:     BackpressureCondition::default(),
            fri_job_manager:        BackpressureCondition::default(),
            gapless_committer:      BackpressureCondition::default(),
            upgrade_gatekeeper:     BackpressureCondition::default(),
            l1_sender_commit:       BackpressureCondition::default(),
            snark_job_manager:      BackpressureCondition::default(),
            gapless_l1_proof_sender: BackpressureCondition::default(),
            l1_sender_prove:        BackpressureCondition::default(),
            priority_tree:          BackpressureCondition::default(),
            l1_sender_execute:      BackpressureCondition::default(),
        }
    }
}

impl PipelineHealthConfig {
    pub fn condition_for(&self, id: ComponentId) -> &BackpressureCondition {
        match id {
            ComponentId::BlockExecutor        => &self.block_executor,
            ComponentId::BlockApplier         => &self.block_applier,
            ComponentId::TreeManager          => &self.tree_manager,
            ComponentId::BlockCanonizer       => &self.block_canonizer,
            ComponentId::ProverInputGenerator => &self.prover_input_generator,
            ComponentId::Batcher              => &self.batcher,
            ComponentId::BatchVerification    => &self.batch_verification,
            ComponentId::FriJobManager        => &self.fri_job_manager,
            ComponentId::GaplessCommitter     => &self.gapless_committer,
            ComponentId::UpgradeGatekeeper    => &self.upgrade_gatekeeper,
            ComponentId::L1SenderCommit       => &self.l1_sender_commit,
            ComponentId::SnarkJobManager      => &self.snark_job_manager,
            ComponentId::GaplessL1ProofSender => &self.gapless_l1_proof_sender,
            ComponentId::L1SenderProve        => &self.l1_sender_prove,
            ComponentId::PriorityTree         => &self.priority_tree,
            ComponentId::L1SenderExecute      => &self.l1_sender_execute,
        }
    }
}
```

- [ ] **Step 4: Create stub `lib/pipeline_health/src/lib.rs`**

```rust
pub mod config;
pub use config::{BackpressureCondition, ComponentId, PipelineHealthConfig};
```

- [ ] **Step 5: Register crate in workspace**

Add to `Cargo.toml` (root workspace members list):
```toml
"lib/pipeline_health",
```

- [ ] **Step 6: Run tests**

Run: `cargo nextest run -p zksync_os_pipeline_health -- config -v`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add lib/pipeline_health/
git commit -m "feat(pipeline_health): add ComponentId, BackpressureCondition, PipelineHealthConfig"
```

---

### Task 4: Implement `PipelineHealthMonitor`

**Files:**
- Create: `lib/pipeline_health/src/monitor.rs`
- Create: `lib/pipeline_health/src/metrics.rs`
- Modify: `lib/pipeline_health/src/lib.rs`

- [ ] **Step 1: Write failing unit tests for `evaluate`**

Create `lib/pipeline_health/src/monitor.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackpressureCondition, ComponentId, PipelineHealthConfig};
    use tokio::sync::watch;
    use tokio::time::Instant;
    use zksync_os_observability::{ComponentHealth, GenericComponentState};
    use zksync_os_types::transaction_acceptance_state::{
        BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
    };
    use std::time::Duration;

    fn make_health(state: GenericComponentState, secs_in_state: u64, last_seq: u64) -> ComponentHealth {
        ComponentHealth {
            state,
            state_entered_at: Instant::now() - Duration::from_secs(secs_in_state),
            last_processed_seq: last_seq,
        }
    }

    fn monitor_with(condition: BackpressureCondition, id: ComponentId) -> PipelineHealthMonitor {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let mut config = PipelineHealthConfig::default();
        match id {
            ComponentId::BlockExecutor  => config.block_executor = condition,
            ComponentId::FriJobManager  => config.fri_job_manager = condition,
            ComponentId::L1SenderCommit => config.l1_sender_commit = condition,
            ComponentId::SnarkJobManager => config.snark_job_manager = condition,
            _ => {}
        }
        let (monitor, _rx) = PipelineHealthMonitor::new(config, stop_rx);
        monitor
    }

    // Test 1: Pipeline-loop in WaitingRecv with block lag → None (idle)
    #[test]
    fn pipeline_loop_waiting_recv_no_trigger() {
        let m = monitor_with(
            BackpressureCondition { max_block_lag: Some(5), ..Default::default() },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingRecv, 0, 0);
        assert!(m.evaluate(ComponentId::L1SenderCommit, &health, 100).is_none());
    }

    // Test 2: Pipeline-loop in Processing with lag → None
    #[test]
    fn pipeline_loop_processing_no_trigger() {
        let m = monitor_with(
            BackpressureCondition { max_block_lag: Some(5), ..Default::default() },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::Processing, 0, 0);
        assert!(m.evaluate(ComponentId::L1SenderCommit, &health, 100).is_none());
    }

    // Test 3: Pipeline-loop in WaitingSend, duration exceeded → WaitingSendTooLong
    #[test]
    fn pipeline_loop_waiting_send_duration_exceeded() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(10)),
                ..Default::default()
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 20, 0);
        let cause = m.evaluate(ComponentId::L1SenderCommit, &health, 0).unwrap();
        assert_eq!(cause.component, "l1_sender_commit");
        assert!(matches!(cause.trigger, BackpressureTrigger::WaitingSendTooLong { .. }));
    }

    // Test 4: Pipeline-loop in WaitingSend, block lag exceeded → BlockLagTooHigh
    #[test]
    fn pipeline_loop_waiting_send_lag_exceeded() {
        let m = monitor_with(
            BackpressureCondition { max_block_lag: Some(10), ..Default::default() },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 0, 80);
        let cause = m.evaluate(ComponentId::L1SenderCommit, &health, 100).unwrap();
        assert_eq!(cause.component, "l1_sender_commit");
        assert!(matches!(
            cause.trigger,
            BackpressureTrigger::BlockLagTooHigh { threshold: 10, actual: 20 }
        ));
    }

    // Test 5: Both exceeded → WaitingSendTooLong takes priority
    #[test]
    fn pipeline_loop_both_exceeded_duration_wins() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(10)),
                max_block_lag: Some(5),
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 20, 80);
        let cause = m.evaluate(ComponentId::L1SenderCommit, &health, 100).unwrap();
        assert!(matches!(cause.trigger, BackpressureTrigger::WaitingSendTooLong { .. }));
    }

    // Test 6: WaitingSend, below thresholds → None
    #[test]
    fn pipeline_loop_waiting_send_below_threshold() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(30)),
                max_block_lag: Some(50),
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 5, 95);
        assert!(m.evaluate(ComponentId::L1SenderCommit, &health, 100).is_none());
    }

    // Test 7: Reactive, any state, lag exceeded → BlockLagTooHigh
    #[test]
    fn reactive_lag_exceeded_regardless_of_state() {
        let m = monitor_with(
            BackpressureCondition { max_block_lag: Some(10), ..Default::default() },
            ComponentId::FriJobManager,
        );
        for state in [
            GenericComponentState::WaitingRecv,
            GenericComponentState::Processing,
            GenericComponentState::ProcessingOrWaitingRecv,
        ] {
            let health = make_health(state, 0, 80);
            let cause = m.evaluate(ComponentId::FriJobManager, &health, 100).unwrap();
            assert!(matches!(cause.trigger, BackpressureTrigger::BlockLagTooHigh { .. }));
        }
    }

    // Test 8: Reactive, lag not exceeded → None
    #[test]
    fn reactive_lag_not_exceeded() {
        let m = monitor_with(
            BackpressureCondition { max_block_lag: Some(50), ..Default::default() },
            ComponentId::FriJobManager,
        );
        let health = make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 95);
        assert!(m.evaluate(ComponentId::FriJobManager, &health, 100).is_none());
    }

    // Test 9: Two components with causes → both in acceptance state
    #[tokio::test]
    async fn two_causes_both_in_acceptance_state() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let mut config = PipelineHealthConfig::default();
        config.fri_job_manager = BackpressureCondition { max_block_lag: Some(5), ..Default::default() };
        config.l1_sender_commit = BackpressureCondition {
            max_waiting_send_duration: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);

        let (_, fri_rx) = zksync_os_observability::ComponentHealthReporter::new("fri_job_manager");
        let (_, l1_rx) = zksync_os_observability::ComponentHealthReporter::new("l1_sender_commit");
        monitor.register(ComponentId::FriJobManager, fri_rx);
        monitor.register(ComponentId::L1SenderCommit, l1_rx);
        // Manually evaluate with forced health values by calling evaluate_and_update
        // with head at 100 and both components lagging.
        monitor.force_health_for_test(ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 80));
        monitor.force_health_for_test(ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 20, 90));
        monitor.evaluate_and_update_with_head(100);

        let state = acceptance_rx.borrow().clone();
        if let TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes }
        ) = state {
            assert_eq!(causes.len(), 2);
        } else {
            panic!("expected NotAccepting(PipelineBackpressure)");
        }
    }

    // Test 10: One cause clears → other remains → still NotAccepting
    #[tokio::test]
    async fn one_cause_clears_other_remains_not_accepting() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let mut config = PipelineHealthConfig::default();
        config.fri_job_manager = BackpressureCondition { max_block_lag: Some(5), ..Default::default() };
        config.l1_sender_commit = BackpressureCondition { max_block_lag: Some(5), ..Default::default() };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);
        // Both lagging at head=100
        monitor.force_health_for_test(ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 80));
        monitor.force_health_for_test(ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 80));
        monitor.evaluate_and_update_with_head(100);
        assert!(matches!(acceptance_rx.borrow().clone(),
            TransactionAcceptanceState::NotAccepting(NotAcceptingReason::PipelineBackpressure { .. })));

        // FriJobManager catches up — L1SenderCommit still lagging
        monitor.force_health_for_test(ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 98));
        monitor.evaluate_and_update_with_head(100);
        let state = acceptance_rx.borrow().clone();
        if let TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes }
        ) = state {
            assert_eq!(causes.len(), 1);
            assert_eq!(causes[0].component, "l1_sender_commit");
        } else {
            panic!("expected NotAccepting with 1 remaining cause");
        }
    }

    // Test 11: All causes clear → Accepting
    #[tokio::test]
    async fn all_causes_clear_becomes_accepting() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let mut config = PipelineHealthConfig::default();
        config.l1_sender_commit = BackpressureCondition { max_block_lag: Some(5), ..Default::default() };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);
        monitor.force_health_for_test(ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 80));
        monitor.evaluate_and_update_with_head(100);
        assert!(matches!(acceptance_rx.borrow().clone(),
            TransactionAcceptanceState::NotAccepting(_)));

        // L1SenderCommit catches up
        monitor.force_health_for_test(ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 98));
        monitor.evaluate_and_update_with_head(100);
        assert_eq!(*acceptance_rx.borrow(), TransactionAcceptanceState::Accepting);
    }

    // Test 12: Lag metrics emit 0 for WaitingRecv components, non-zero for WaitingSend
    #[tokio::test]
    async fn metrics_zero_for_idle_nonzero_for_waiting_send() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let config = PipelineHealthConfig::default();
        let (mut monitor, _) = PipelineHealthMonitor::new(config, stop_rx);
        // Idle component: WaitingRecv with lag should emit lag=0
        monitor.force_health_for_test(ComponentId::BlockApplier,
            make_health(GenericComponentState::WaitingRecv, 0, 50));
        // Stuck component: WaitingSend with lag should emit lag>0
        monitor.force_health_for_test(ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 5, 50));
        // Calling emit_metrics indirectly via evaluate_and_update_with_head
        monitor.evaluate_and_update_with_head(100);
        // Verify via Prometheus: component_block_lag{block_applier}==0,
        // component_block_lag{l1_sender_commit}==50, component_waiting_send_seconds{l1_sender_commit}>0.
        // (Reading metric values is framework-specific — verify by checking MONITOR_METRICS directly
        // if vise provides a test accessor, or accept this as a smoke test that the code runs.)
    }
}
```

> **Note:** The `force_health_for_test` helper and `evaluate_and_update_with_head` need to be added as `#[cfg(test)]` methods on `PipelineHealthMonitor`. See the implementation step for how to add them cleanly.

Run: `cargo nextest run -p zksync_os_pipeline_health -- monitor -v`
Expected: FAIL — types not defined

- [ ] **Step 2: Implement Prometheus metrics**

Create `lib/pipeline_health/src/metrics.rs`:

```rust
use crate::config::ComponentId;
use vise::{EncodeLabelSet, EncodeLabelValue, Family, Gauge, Metrics};

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct ComponentLabel {
    pub component: &'static str,
}

impl From<ComponentId> for ComponentLabel {
    fn from(id: ComponentId) -> Self {
        Self { component: id.as_str() }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct MonitorMetrics {
    /// 1 if this component is currently an active backpressure cause, else 0.
    pub backpressure_active: Family<ComponentLabel, Gauge<u64>>,
    /// Blocks behind pipeline head. 0 when component is idle (WaitingRecv/Processing).
    pub component_block_lag: Family<ComponentLabel, Gauge<u64>>,
    /// Seconds the component has been in WaitingSend. 0 when not in WaitingSend.
    pub component_waiting_send_seconds: Family<ComponentLabel, Gauge<f64>>,
}

#[vise::register]
pub static MONITOR_METRICS: vise::Global<MonitorMetrics> = vise::Global::new();
```

- [ ] **Step 3: Implement `PipelineHealthMonitor`**

Complete `lib/pipeline_health/src/monitor.rs`:

```rust
use crate::config::{ComponentId, PipelineHealthConfig};
use crate::metrics::{ComponentLabel, MONITOR_METRICS};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, Instant};
use zksync_os_observability::{ComponentHealth, GenericComponentState};
use zksync_os_types::transaction_acceptance_state::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

pub struct PipelineHealthMonitor {
    config: PipelineHealthConfig,
    components: Vec<(ComponentId, watch::Receiver<ComponentHealth>)>,
    acceptance_tx: watch::Sender<TransactionAcceptanceState>,
    stop_receiver: watch::Receiver<bool>,
}

impl PipelineHealthMonitor {
    pub fn new(
        config: PipelineHealthConfig,
        stop_receiver: watch::Receiver<bool>,
    ) -> (Self, watch::Receiver<TransactionAcceptanceState>) {
        // Panic early rather than panicking inside tokio::time::interval at runtime.
        assert!(
            config.eval_interval > std::time::Duration::ZERO,
            "PipelineHealthConfig::eval_interval must be > 0"
        );
        let (acceptance_tx, acceptance_rx) =
            watch::channel(TransactionAcceptanceState::Accepting);
        (
            Self {
                config,
                components: vec![],
                acceptance_tx,
                stop_receiver,
            },
            acceptance_rx,
        )
    }

    /// Register a component's health receiver. Call during pipeline wiring
    /// for each component present in this node role.
    pub fn register(&mut self, id: ComponentId, receiver: watch::Receiver<ComponentHealth>) {
        self.components.push((id, receiver));
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.config.eval_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => self.evaluate_and_update(),
                _ = self.stop_receiver.changed() => {
                    tracing::info!("PipelineHealthMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn head_seq(&self) -> u64 {
        self.components
            .iter()
            .find(|(id, _)| *id == ComponentId::BlockExecutor)
            .map(|(_, rx)| rx.borrow().last_processed_seq)
            .unwrap_or(0)
    }

    fn evaluate_and_update(&self) {
        let head_seq = self.head_seq();
        self.evaluate_and_update_with_head(head_seq);
    }

    fn evaluate_and_update_with_head(&self, head_seq: u64) {
        let mut active_causes: Vec<BackpressureCause> = self.components
            .iter()
            .filter_map(|(id, rx)| self.evaluate(*id, &rx.borrow(), head_seq))
            .collect();

        // Deterministic ordering: stable watch value when conditions haven't changed.
        active_causes.sort_by_key(|c| c.component);

        self.emit_metrics(&active_causes, head_seq);

        let new_state = if active_causes.is_empty() {
            TransactionAcceptanceState::Accepting
        } else {
            TransactionAcceptanceState::NotAccepting(
                NotAcceptingReason::PipelineBackpressure { causes: active_causes },
            )
        };

        // send_if_modified: only wakes watchers when acceptance state actually changes.
        self.acceptance_tx.send_if_modified(|current| {
            if *current == new_state {
                return false;
            }
            match &new_state {
                TransactionAcceptanceState::NotAccepting(reason) =>
                    tracing::warn!(?reason, "pipeline backpressure: stopping transaction acceptance"),
                TransactionAcceptanceState::Accepting =>
                    tracing::info!("pipeline backpressure cleared: resuming transaction acceptance"),
            }
            *current = new_state.clone();
            true
        });
    }

    /// Evaluate a single component. Returns `Some(cause)` if a condition is triggered.
    pub(crate) fn evaluate(
        &self,
        id: ComponentId,
        health: &ComponentHealth,
        head_seq: u64,
    ) -> Option<BackpressureCause> {
        let condition = self.config.condition_for(id);
        let now = Instant::now();

        if id.is_reactive() {
            if let Some(max_lag) = condition.max_block_lag {
                let lag = head_seq.saturating_sub(health.last_processed_seq);
                if lag > max_lag {
                    return Some(BackpressureCause {
                        component: id.as_str(),
                        trigger: BackpressureTrigger::BlockLagTooHigh {
                            threshold: max_lag,
                            actual: lag,
                        },
                    });
                }
            }
            return None;
        }

        // Pipeline-loop: both conditions require WaitingSend.
        if health.state != GenericComponentState::WaitingSend {
            return None;
        }

        if let Some(max_duration) = condition.max_waiting_send_duration {
            let elapsed = now.duration_since(health.state_entered_at);
            if elapsed > max_duration {
                return Some(BackpressureCause {
                    component: id.as_str(),
                    trigger: BackpressureTrigger::WaitingSendTooLong {
                        threshold: max_duration,
                        actual: elapsed,
                    },
                });
            }
        }

        if let Some(max_lag) = condition.max_block_lag {
            let lag = head_seq.saturating_sub(health.last_processed_seq);
            if lag > max_lag {
                return Some(BackpressureCause {
                    component: id.as_str(),
                    trigger: BackpressureTrigger::BlockLagTooHigh {
                        threshold: max_lag,
                        actual: lag,
                    },
                });
            }
        }

        None
    }

    fn emit_metrics(&self, active_causes: &[BackpressureCause], head_seq: u64) {
        for (id, rx) in &self.components {
            let health = rx.borrow();
            let label = ComponentLabel::from(*id);
            let is_active = active_causes.iter().any(|c| c.component == id.as_str());
            MONITOR_METRICS.backpressure_active[&label].set(is_active as u64);

            let (lag, waiting_send_secs) = if id.is_reactive() {
                (head_seq.saturating_sub(health.last_processed_seq), 0.0_f64)
            } else if health.state == GenericComponentState::WaitingSend {
                let lag = head_seq.saturating_sub(health.last_processed_seq);
                let secs = Instant::now()
                    .duration_since(health.state_entered_at)
                    .as_secs_f64();
                (lag, secs)
            } else {
                (0, 0.0)
            };

            MONITOR_METRICS.component_block_lag[&label].set(lag);
            MONITOR_METRICS.component_waiting_send_seconds[&label].set(waiting_send_secs);
        }
    }

    /// Test helper: override the watch receiver for a component with a fixed health value.
    /// Only available in tests.
    #[cfg(test)]
    pub fn force_health_for_test(&mut self, id: ComponentId, health: ComponentHealth) {
        let (tx, rx) = watch::channel(health);
        std::mem::forget(tx); // keep sender alive so receiver is valid
        if let Some(entry) = self.components.iter_mut().find(|(cid, _)| *cid == id) {
            entry.1 = rx;
        } else {
            self.components.push((id, rx));
        }
    }
}
```

- [ ] **Step 4: Update `lib/pipeline_health/src/lib.rs`**

```rust
pub mod config;
pub mod metrics;
pub mod monitor;

pub use config::{BackpressureCondition, ComponentId, PipelineHealthConfig};
pub use monitor::PipelineHealthMonitor;
```

- [ ] **Step 5: Run the unit tests**

Run: `cargo nextest run -p zksync_os_pipeline_health -v`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add lib/pipeline_health/
git commit -m "feat(pipeline_health): implement PipelineHealthMonitor with evaluate and metrics"
```

---

## Chunk 3: Component Migration

### Task 5: Migrate `BlockExecutor`

**Files:**
- Modify: `lib/sequencer/src/execution/block_executor.rs`

- [ ] **Step 1: Read the file**

```bash
cat lib/sequencer/src/execution/block_executor.rs
```

Find where `ComponentStateReporter::global().handle_for(...)` is called and where `latency_tracker.enter_state(SequencerState::WaitingSend)` + `output.send(...)` occur.

- [ ] **Step 2: Replace `ComponentStateReporter` with `ComponentHealthReporter`**

Change the struct definition to add a `health_reporter` field (replacing the `ComponentStateReporter` handle):

```rust
// In BlockExecutor::run() or the method that calls ComponentStateReporter::global()
// BEFORE:
let latency_tracker = ComponentStateReporter::global()
    .handle_for("block_executor", SequencerState::WaitingForCommand);

// AFTER: (reporter is passed in as a field on BlockExecutor)
// BlockExecutor must receive a ComponentHealthReporter in its struct or constructor.
// Add to BlockExecutor:
pub health_reporter: ComponentHealthReporter,
```

The key change in the loop body: every `latency_tracker.enter_state(SequencerState::X)` must map to a `GenericComponentState`. For `SequencerState::WaitingSend` → `GenericComponentState::WaitingSend`. For all execution states and `WaitingForCommand` → `GenericComponentState::Processing`. (The exact mapping depends on which states exist — check the `SequencerState` enum.)

After `output.send(...).await?` when sending downstream, add:
```rust
self.health_reporter.record_processed(block_output.header.number.as_u64());
```

> **Note:** Read the actual `block_executor.rs` before making changes. The `SequencerState` has many variants — map each to the appropriate `GenericComponentState`. `WaitingForCommand` → `WaitingRecv`, `WaitingSend` → `WaitingSend`, everything else → `Processing`.

- [ ] **Step 3: Build `lib/sequencer`**

Run: `cargo build -p zksync_os_sequencer 2>&1`
Expected: Compiles

- [ ] **Step 4: Run unit tests**

Run: `cargo nextest run -p zksync_os_sequencer --release`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add lib/sequencer/src/execution/block_executor.rs
git commit -m "feat(sequencer): migrate BlockExecutor to ComponentHealthReporter"
```

---

### Task 6: Migrate remaining components (batch)

**Files to modify:**
- `lib/sequencer/src/` (find `BlockCanonizer` / canonizer file)
- `node/bin/src/block_applier.rs` (or wherever BlockApplier lives)
- `node/bin/src/tree_manager.rs`
- `node/bin/src/batcher/mod.rs`
- `node/bin/src/prover_input_generator/mod.rs`
- `node/bin/src/prover_api/fri_job_manager.rs`
- `node/bin/src/prover_api/snark_job_manager.rs`
- `node/bin/src/prover_api/gapless_committer.rs`
- `node/bin/src/prover_api/gapless_l1_proof_sender.rs`
- `lib/l1_sender/src/lib.rs`
- `lib/l1_sender/src/upgrade_gatekeeper.rs`
- `lib/batch_verification/src/client/mod.rs`
- `lib/batch_verification/src/sequencer/component.rs`
- `lib/priority_tree/src/lib.rs`
- `lib/revm_consistency_checker/src/node.rs`

> These 15 components all follow the same migration pattern as `BlockExecutor`. Do them in one task to avoid 15 separate commits, but commit after every 3-4 components.
>
> **RevmConsistencyChecker special case:** `lib/revm_consistency_checker/src/node.rs` uses `ComponentStateReporter` but `RevmConsistencyChecker` has no entry in `ComponentId`. It is an optional consistency checker, not a pipeline component. Migrate it to `ComponentHealthReporter` for the time-in-state metrics benefit, but **do NOT register its receiver with `PipelineHealthMonitor`** — it is not a backpressure source. Pass `None` to the monitor registration at wiring time (simply omit the `monitor.register()` call for it). The `record_processed` call is also optional for this component if it has no meaningful block sequence number.

- [ ] **Step 1: Establish baseline (TDD pre-condition)**

Before touching any files, run the unit tests to confirm they all pass:

```bash
cargo nextest run --workspace --exclude zksync_os_integration_tests --release 2>&1 | tail -5
```

Expected: All pass. This is the green baseline. After removing `ComponentStateReporter` calls the build will fail — that failing build is our "red" state to fix.

- [ ] **Step 2: Read all component files to understand their `ComponentStateReporter` usage**

For each file, locate:
1. `ComponentStateReporter::global().handle_for(component_name, initial_state)` call
2. Every `latency_tracker.enter_state(X)` call
3. The `output.send(item).await` call where `record_processed(seq)` should be added
4. What sequence number to use (block number, or highest block in batch)

For batch-level components (`Batcher`, `FriJobManager`, `SnarkJobManager`, etc.): use the highest block number in the last processed batch as the seq.

- [ ] **Step 3: For each component, apply the migration pattern**

```rust
// Pattern: Remove
let latency_tracker = ComponentStateReporter::global()
    .handle_for("component_name", InitialState::X);

// Pattern: Replace with (reporter field injected via struct constructor)
// The struct gets a new field:
health_reporter: ComponentHealthReporter,

// In the loop, enter_state mapping:
// Component-specific state → GenericComponentState
// WaitingRecv / WaitingForCommand → WaitingRecv
// WaitingSend → WaitingSend
// Everything else (processing, connecting, etc.) → Processing

// After successful downstream send:
self.health_reporter.record_processed(last_block_in_item);
```

For the `state_label_to_generic` mapping, examine each component's state enum:
- `L1SenderState::WaitingRecv` → `WaitingRecv`
- `L1SenderState::WaitingSend` → `WaitingSend`
- `L1SenderState::SendingToL1`, `WaitingL1Inclusion` → `Processing`

> `FriJobManager` uses `ProcessingOrWaitingRecv` as its ambient state (it never enters `WaitingSend`). Its `enter_state` calls should use `GenericComponentState::ProcessingOrWaitingRecv` for its normal operating state.

- [ ] **Step 4: Update each component's constructor to accept `ComponentHealthReporter`**

Each component that previously called `ComponentStateReporter::global()` internally must now accept a `ComponentHealthReporter` as a constructor parameter. The caller (wiring code in `node/bin/src/lib.rs`) will create the reporter and pass it in.

The pattern at the construction site:

```rust
let (health_reporter, health_rx) = ComponentHealthReporter::new("component_name");
monitor.register(ComponentId::ComponentName, health_rx);
let component = ComponentName { health_reporter, ... };
```

- [ ] **Step 5: Build after every 3-4 components**

Run: `cargo build --workspace 2>&1 | head -30`
Fix any compile errors before moving on.

- [ ] **Step 6: Run unit tests after all 15 components are migrated**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add node/bin/src/ lib/l1_sender/ lib/batch_verification/ lib/priority_tree/ \
  lib/revm_consistency_checker/ lib/sequencer/src/
git commit -m "feat: migrate all pipeline components to ComponentHealthReporter"
```

---

### Task 7: Remove `ComponentStateReporter`

**Files:**
- Delete: `lib/observability/src/component_state_reporter.rs`
- Modify: `lib/observability/src/lib.rs`

- [ ] **Step 1: Verify no remaining usages**

```bash
grep -rn "ComponentStateReporter\|ComponentStateHandle\|BackpressureHandle" \
  --include="*.rs" lib/ node/ zksync_os_integration_tests/
```

Expected: zero results (only the definition files themselves, which we're about to delete)

- [ ] **Step 2: Remove the old reporter export from `lib/observability/src/lib.rs`**

Delete the line:
```rust
pub use component_state_reporter::{ComponentStateHandle, ComponentStateReporter, StateLabel};
```

- [ ] **Step 3: Delete the old file**

```bash
rm lib/observability/src/component_state_reporter.rs
```

- [ ] **Step 4: Build**

Run: `cargo build --workspace 2>&1 | head -30`
Expected: Compiles without errors

- [ ] **Step 5: Run unit tests**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add lib/observability/src/
git commit -m "refactor(observability): remove ComponentStateReporter (replaced by ComponentHealthReporter)"
```

---

## Chunk 4: Node Wiring and RPC Layer

### Task 8: Wire `PipelineHealthMonitor` in `node/bin/src/lib.rs`

**Files:**
- Modify: `node/bin/src/lib.rs`
- Modify: `node/bin/Cargo.toml`

- [ ] **Step 1: Read `node/bin/src/lib.rs`**

```bash
cat node/bin/src/lib.rs
```

Identify:
1. Where `watch::Sender<TransactionAcceptanceState>` is created for `BlockExecutor`
2. Where `TxHandler` is constructed (where `acceptance_state` receiver is passed)
3. Where components are constructed (the `JoinSet` or sequential wiring section)
4. Where `run_status_server` is called

- [ ] **Step 2: Add `zksync_os_pipeline_health` to `node/bin/Cargo.toml`**

```toml
zksync_os_pipeline_health = { path = "../../lib/pipeline_health" }
```

- [ ] **Step 3: Create the monitor and register all components**

In the wiring section, after all components are constructed but before spawning tasks:

```rust
use zksync_os_pipeline_health::{ComponentId, PipelineHealthConfig, PipelineHealthMonitor};
use zksync_os_observability::ComponentHealthReporter;

// PipelineHealthMonitor owns the acceptance state sender for backpressure.
// config.pipeline_health is a PipelineHealthConfig loaded from node config.
let (mut pipeline_monitor, acceptance_rx_for_rpc) =
    PipelineHealthMonitor::new(config.pipeline_health.clone(), stop_receiver.clone());

// Register all components that are present in this node role.
// (Components absent in external-node mode are simply not registered.)
// Collect health receivers for both the monitor AND the status server.
let mut health_entries: Vec<(ComponentId, watch::Receiver<ComponentHealth>)> = vec![];

macro_rules! register {
    ($id:expr, $rx:expr) => {{
        // Clone rx for monitor; original goes to health_entries for status server.
        let (a, b) = ($rx.clone(), $rx);
        pipeline_monitor.register($id, a);
        health_entries.push(($id, b));
    }};
}

register!(ComponentId::BlockExecutor, block_executor_health_rx);
register!(ComponentId::BlockApplier, block_applier_health_rx);
register!(ComponentId::TreeManager, tree_manager_health_rx);
// ... add all other present components ...

// Shared between status server and (if needed) other consumers.
let component_health = std::sync::Arc::new(health_entries);

// Spawn the monitor as a pipeline task.
task_set.spawn(async move { pipeline_monitor.run().await; Ok(()) });
```

- [ ] **Step 4: Make `TxHandler` check BOTH acceptance sources**

`TxHandler` must check two independent signals:
1. The monitor's receiver — for `PipelineBackpressure` causes
2. `BlockExecutor`'s receiver — for `BlockProductionDisabled` (unchanged mechanism)

Update `TxHandler`:

```rust
pub struct TxHandler<RpcStorage, Mempool> {
    config: RpcConfig,
    storage: RpcStorage,
    mempool: Mempool,
    /// From PipelineHealthMonitor. Signals PipelineBackpressure.
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    /// From BlockExecutor. Signals BlockProductionDisabled (max_blocks_to_produce halt).
    block_executor_acceptance: watch::Receiver<TransactionAcceptanceState>,
    tx_forwarder: Option<DynProvider>,
}

async fn send_raw_transaction_impl(&self, tx_bytes: Bytes) -> Result<B256, EthSendRawTransactionError> {
    // Check monitor backpressure first.
    if let TransactionAcceptanceState::NotAccepting(reason) = &*self.acceptance_state.borrow() {
        return Err(EthSendRawTransactionError::NotAcceptingTransactions(reason.clone()));
    }
    // Then check block-executor's BlockProductionDisabled signal.
    if let TransactionAcceptanceState::NotAccepting(reason) = &*self.block_executor_acceptance.borrow() {
        return Err(EthSendRawTransactionError::NotAcceptingTransactions(reason.clone()));
    }
    // ... rest unchanged
}
```

Update wiring call:
```rust
let tx_handler = TxHandler::new(
    ...,
    acceptance_rx_for_rpc.clone(),          // monitor's receiver
    block_executor.tx_acceptance_state_sender.subscribe(), // BlockExecutor's receiver
    ...
);
```

Update `TxHandler::new` signature to accept both receivers and store both.

- [ ] **Step 5: Build `TxHandler`**

Run: `cargo build -p zksync_os_rpc 2>&1 | head -30`
Expected: Compiles.

- [ ] **Step 6: Check for `BackpressureHandle::global()` in `tx_handler.rs`**

```bash
grep -n "BackpressureHandle\|global()" lib/rpc/src/tx_handler.rs
```

If found, remove it. The two-receiver check above replaces it entirely.

- [ ] **Step 7: Pass `component_health` to `run_status_server`**

```rust
run_status_server(
    config.status_server.address.clone(),
    stop_receiver.clone(),
    acceptance_rx_for_rpc.clone(),
    Arc::clone(&component_health),
)
```

- [ ] **Step 8: Build**

Run: `cargo build --workspace 2>&1 | head -30`
Expected: Compiles without errors

- [ ] **Step 9: Run unit tests**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 10: Commit**

```bash
git add node/bin/src/lib.rs node/bin/Cargo.toml lib/rpc/src/tx_handler.rs
git commit -m "feat(node): wire PipelineHealthMonitor, dual acceptance checks in TxHandler"
```

---

### Task 9: Add `PipelineHealthConfig` to node config

**Files:**
- Modify: `node/bin/src/config/mod.rs` (real config location — NOT `node/sequencer/config.rs`)

- [ ] **Step 1: Read the config file and discover the derive framework**

```bash
cat node/bin/src/config/mod.rs | head -80
```

This codebase uses a custom config framework. The struct likely has `#[derive(DescribeConfig, DeserializeConfig)]` (not `#[derive(serde::Deserialize)]`) and fields use `#[config(default)]` (not `#[serde(default)]`). Confirm this before proceeding.

Also check if there is already a `BackpressureCondition` or duration config pattern elsewhere in the config file — follow that precedent.

- [ ] **Step 2: Add the required derives to `PipelineHealthConfig` and `BackpressureCondition`**

In `lib/pipeline_health/src/config.rs`, add whatever derives the config framework requires. Based on the existing pattern in `node/bin/src/config/mod.rs`:

```rust
// If the framework uses DescribeConfig/DeserializeConfig:
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct PipelineHealthConfig {
    #[config(default)]
    pub eval_interval: Duration, // default is Duration::from_secs(1) via our impl Default
    #[config(default)]
    pub block_executor: BackpressureCondition,
    // ... all fields ...
}

#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct BackpressureCondition {
    pub max_waiting_send_duration: Option<Duration>,
    pub max_block_lag: Option<u64>,
}
```

> **Note:** If the framework is serde-based after all (check by looking at what derives other config structs use), use `#[serde(default)]` and `#[derive(serde::Deserialize)]` instead. Match exactly what exists in the codebase — do not guess.

- [ ] **Step 3: Add `pipeline_health` field to the main config struct**

In `node/bin/src/config/mod.rs`:

```rust
use zksync_os_pipeline_health::PipelineHealthConfig;

// Add to the config struct (exact attribute syntax depends on framework found in Step 1):
#[config(default)]  // or #[serde(default)]
pub pipeline_health: PipelineHealthConfig,
```

- [ ] **Step 4: Build and run unit tests**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add node/bin/src/config/mod.rs lib/pipeline_health/src/config.rs
git commit -m "feat(config): add PipelineHealthConfig to node config"
```

---

## Chunk 5: Status Server Extension

### Task 10: Extend `/status/health` with pipeline snapshot

**Files:**
- Modify: `lib/status/src/lib.rs`
- Modify: `lib/status/src/health.rs`
- Modify: `lib/status/Cargo.toml`

- [ ] **Step 1: Add dependencies to `lib/status/Cargo.toml`**

```toml
zksync_os_pipeline_health = { path = "../pipeline_health" }
zksync_os_types = { path = "../types" }
indexmap = { workspace = true }
```

(Check if these are already present; add only what's missing.)

- [ ] **Step 2: Write a failing test for the new handler response format**

Add a test to `lib/status/src/health.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use std::sync::Arc;
    use tokio::sync::watch;
    use zksync_os_observability::{ComponentHealth, ComponentHealthReporter, GenericComponentState};
    use zksync_os_pipeline_health::ComponentId;
    use zksync_os_types::transaction_acceptance_state::TransactionAcceptanceState;

    fn make_state() -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, health_rx) = ComponentHealthReporter::new("block_executor");
        reporter.record_processed(12345);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![(ComponentId::BlockExecutor, health_rx)]),
        }
    }

    #[tokio::test]
    async fn healthy_node_returns_200() {
        let state = axum::extract::State(make_state());
        let (status, Json(body)) = health(state).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.healthy);
        assert!(body.accepting_transactions);
        assert!(body.backpressure_causes.is_empty());
        assert_eq!(body.pipeline.head_block, 12345);
    }

    #[tokio::test]
    async fn terminating_node_returns_503() {
        let mut state = make_state();
        let (stop_tx, _) = watch::channel(true);
        // replace stop receiver with one that reads true
        let (_tx2, rx2) = watch::channel(true);
        state.stop_receiver = rx2;
        let state = axum::extract::State(state);
        let (status, Json(body)) = health(state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body.healthy);
    }

    #[tokio::test]
    async fn backpressure_returns_503_with_causes() {
        use zksync_os_types::transaction_acceptance_state::{
            BackpressureCause, BackpressureTrigger, NotAcceptingReason,
        };
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockLagTooHigh { threshold: 500, actual: 782 },
        };
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes: vec![cause] },
        ));
        state.acceptance_state = rx;
        let state = axum::extract::State(state);
        let (status, Json(body)) = health(state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body.accepting_transactions);
        assert_eq!(body.backpressure_causes.len(), 1);
        assert_eq!(body.backpressure_causes[0].component, "fri_job_manager");
        assert_eq!(body.backpressure_causes[0].trigger, "block_lag_too_high");
        assert_eq!(body.backpressure_causes[0].threshold_blocks, Some(500));
        assert_eq!(body.backpressure_causes[0].actual_blocks, Some(782));
    }
}
```

Run: `cargo nextest run -p zksync_os_status -v`
Expected: FAIL — AppState missing fields

- [ ] **Step 3: Extend `AppState` in `lib/status/src/lib.rs`**

```rust
use std::sync::Arc;
use tokio::sync::watch;
use zksync_os_observability::ComponentHealth;
use zksync_os_pipeline_health::ComponentId;
use zksync_os_types::transaction_acceptance_state::TransactionAcceptanceState;

#[derive(Clone)]
struct AppState {
    stop_receiver: watch::Receiver<bool>,
    /// Acceptance state from PipelineHealthMonitor.
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    /// One entry per registered component.
    component_health: Arc<Vec<(ComponentId, watch::Receiver<ComponentHealth>)>>,
}

pub async fn run_status_server(
    bind_address: String,
    stop_receiver: watch::Receiver<bool>,
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    component_health: Arc<Vec<(ComponentId, watch::Receiver<ComponentHealth>)>>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .with_state(AppState { stop_receiver, acceptance_state, component_health });
    // ... rest unchanged
}
```

- [ ] **Step 4: Replace `lib/status/src/health.rs`**

```rust
use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use indexmap::IndexMap;
use serde::Serialize;
use tokio::time::Instant;
use zksync_os_observability::GenericComponentState;
use zksync_os_pipeline_health::ComponentId;
use zksync_os_types::transaction_acceptance_state::{
    BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

#[derive(Serialize)]
pub struct HealthResponse {
    pub healthy: bool,
    pub accepting_transactions: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backpressure_causes: Vec<BackpressureCauseJson>,
    pub pipeline: PipelineSnapshot,
}

#[derive(Serialize)]
pub struct PipelineSnapshot {
    pub head_block: u64,
    pub components: IndexMap<&'static str, ComponentSnapshot>,
}

#[derive(Serialize)]
pub struct ComponentSnapshot {
    pub state: &'static str,
    pub state_duration_secs: f64,
    pub last_processed_block: u64,
    pub block_lag: u64,
    pub waiting_send_secs: f64,
}

#[derive(Serialize)]
pub struct BackpressureCauseJson {
    pub component: &'static str,
    pub trigger: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_blocks: Option<u64>,
}

pub(crate) async fn health(
    State(state): State<AppState>,
) -> (StatusCode, Json<HealthResponse>) {
    let is_terminating = *state.stop_receiver.borrow();
    let acceptance = state.acceptance_state.borrow().clone();
    let accepting = matches!(acceptance, TransactionAcceptanceState::Accepting);

    let head_block = state.component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| rx.borrow().last_processed_seq)
        .unwrap_or(0);

    let mut components: IndexMap<&'static str, ComponentSnapshot> = IndexMap::new();
    for (id, rx) in state.component_health.iter() {
        let h = rx.borrow();
        let elapsed = h.state_entered_at.elapsed().as_secs_f64();
        let lag = head_block.saturating_sub(h.last_processed_seq);
        let waiting_send_secs = if h.state == GenericComponentState::WaitingSend {
            elapsed
        } else {
            0.0
        };
        let block_lag = if h.state == GenericComponentState::WaitingSend || id.is_reactive() {
            lag
        } else {
            0
        };
        components.insert(id.as_str(), ComponentSnapshot {
            state: h.state.as_str(),
            state_duration_secs: elapsed,
            last_processed_block: h.last_processed_seq,
            block_lag,
            waiting_send_secs,
        });
    }

    let backpressure_causes = match &acceptance {
        TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes }
        ) => causes.iter().map(|c| {
            match &c.trigger {
                BackpressureTrigger::WaitingSendTooLong { threshold, actual } =>
                    BackpressureCauseJson {
                        component: c.component,
                        trigger: "waiting_send_too_long",
                        threshold_secs: Some(threshold.as_secs_f64()),
                        actual_secs: Some(actual.as_secs_f64()),
                        threshold_blocks: None,
                        actual_blocks: None,
                    },
                BackpressureTrigger::BlockLagTooHigh { threshold, actual } =>
                    BackpressureCauseJson {
                        component: c.component,
                        trigger: "block_lag_too_high",
                        threshold_secs: None,
                        actual_secs: None,
                        threshold_blocks: Some(*threshold),
                        actual_blocks: Some(*actual),
                    },
            }
        }).collect(),
        _ => vec![],
    };

    let healthy = !is_terminating && accepting;
    let status = if healthy { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };

    (status, Json(HealthResponse {
        healthy,
        accepting_transactions: accepting,
        backpressure_causes,
        pipeline: PipelineSnapshot { head_block, components },
    }))
}
```

> **Note:** `GenericComponentState` needs an `as_str()` method. Check if it already exists — if not, add it:
> ```rust
> impl GenericComponentState {
>     pub fn as_str(self) -> &'static str {
>         match self {
>             Self::WaitingRecv => "waiting_recv",
>             Self::Processing => "processing",
>             Self::WaitingSend => "waiting_send",
>             Self::ProcessingOrWaitingRecv => "processing_or_waiting_recv",
>         }
>     }
> }
> ```

- [ ] **Step 5: Verify `run_status_server` call site in `node/bin/src/lib.rs`**

Task 8 Step 7 already wires this. Confirm the call passes `Arc::clone(&component_health)` and `acceptance_rx_for_rpc.clone()`. No additional changes needed if Task 8 was completed first.

- [ ] **Step 6: Run the tests**

Run: `cargo nextest run -p zksync_os_status -v`
Expected: PASS

- [ ] **Step 7: Run full workspace tests**

Run: `cargo nextest run --workspace --exclude zksync_os_integration_tests --release`
Expected: All pass

- [ ] **Step 8: Commit**

```bash
git add lib/status/ node/bin/src/lib.rs
git commit -m "feat(status): extend /status/health with pipeline snapshot and backpressure causes"
```

---

## Chunk 6: Integration Test

### Task 11: Add integration test for end-to-end backpressure

**Files:**
- Modify: `zksync_os_integration_tests/` (find the appropriate test file for RPC behavior)

- [ ] **Step 1: Find the existing integration test structure**

```bash
ls zksync_os_integration_tests/src/
grep -rn "send_raw_transaction\|NodeConfig\|TestNode" \
  zksync_os_integration_tests/src/ --include="*.rs" | head -20
```

Understand how `TestNode` is constructed and how transactions are submitted in existing tests.

- [ ] **Step 2: Write the failing test**

Add to the appropriate test file (e.g., `zksync_os_integration_tests/src/rpc_tests.rs` or create `zksync_os_integration_tests/src/backpressure_tests.rs`):

```rust
use std::time::{Duration, Instant};
use zksync_os_pipeline_health::{BackpressureCondition, ComponentId, PipelineHealthConfig};
// import TestNode, NodeConfig, FakeProverConfig from the existing test helpers

#[tokio::test]
async fn backpressure_stops_and_resumes_transaction_acceptance() {
    let node = TestNode::start(NodeConfig {
        fake_fri_provers: FakeProverConfig {
            compute_time: Duration::from_secs(10), // prover is slow
            ..Default::default()
        },
        pipeline_health: PipelineHealthConfig {
            fri_job_manager: BackpressureCondition {
                // Trigger as soon as FriJobManager is 1 block behind head.
                // Guaranteed once blocks are produced and a batch enters proving.
                max_block_lag: Some(1),
                ..Default::default()
            },
            eval_interval: Duration::from_millis(200),
            ..Default::default()
        },
        ..default_test_config()
    })
    .await;

    // Submit enough transactions to trigger a batch and push it to FriJobManager.
    node.submit_transactions(50).await;

    // Wait for the monitor to detect the lag (eval_interval * 3 for safety).
    tokio::time::sleep(Duration::from_millis(800)).await;

    // RPC must reject with -32003.
    let err = node.send_raw_transaction(dummy_tx()).await
        .expect_err("expected rejection while under backpressure");
    assert_eq!(err.code, -32003, "expected Transaction Rejected code");
    let data = err.data.as_object().expect("expected JSON object in error data");
    assert_eq!(data["reason"].as_str(), Some("pipeline_backpressure"));
    let causes = data["causes"].as_array().expect("expected causes array");
    assert!(!causes.is_empty());
    let cause = &causes[0];
    assert_eq!(cause["component"].as_str(), Some("fri_job_manager"));
    assert_eq!(cause["trigger"].as_str(), Some("block_lag_too_high"));
    assert!(cause["threshold_blocks"].as_u64().is_some());
    assert!(cause["actual_blocks"].as_u64().is_some());

    // Poll until the prover finishes and lag clears (avoids fixed-sleep fragility).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for backpressure to clear"
        );
        if node.send_raw_transaction(dummy_tx()).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Verify /status/health also reflects the cleared state.
    let health = node.get_health().await;
    assert!(health["healthy"].as_bool().unwrap_or(false));
    assert!(health["accepting_transactions"].as_bool().unwrap_or(false));
}
```

Run: `cargo nextest run -p zksync_os_integration_tests -- backpressure -v`
Expected: FAIL — test infrastructure missing or test logic fails

- [ ] **Step 3: Adapt test to match actual test helpers**

Read the existing integration test helpers to understand `TestNode`, `NodeConfig`, `FakeProverConfig`, etc. Adjust the test to use the exact API those helpers provide. Common adjustments:
- `dummy_tx()` → whatever the existing tests use for a dummy transaction
- `node.get_health()` → might need to be added or already exist
- `default_test_config()` → check the exact function name

- [ ] **Step 4: Run the integration test**

Run: `cargo nextest run -p zksync_os_integration_tests -- backpressure_stops_and_resumes -v`
Expected: PASS

- [ ] **Step 5: Run all integration tests**

Run: `cargo nextest run -p zksync_os_integration_tests`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add zksync_os_integration_tests/
git commit -m "test: add integration test for pipeline backpressure stops and resumes RPC acceptance"
```

---

## Final Verification

### Task 12: Full pre-PR checks

- [ ] **Step 1: Format**

Run: `cargo fmt --all --check`
Expected: No formatting issues. If there are any, run `cargo fmt --all` and commit.

- [ ] **Step 2: Lint**

Run: `cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: No warnings. Fix any clippy warnings before proceeding.

- [ ] **Step 3: Unit tests**

Run: `cargo nextest run --release --workspace --exclude zksync_os_integration_tests`
Expected: All pass

- [ ] **Step 4: Integration tests**

Run: `cargo nextest run -p zksync_os_integration_tests`
Expected: All pass

- [ ] **Step 5: Verify `/status/health` response manually (optional but recommended)**

Start the server locally:
```bash
./run_local.sh ./local-chains/v30.2/default
```

```bash
curl -s http://localhost:{STATUS_PORT}/status/health | jq .
```

Expected: JSON with `healthy: true`, `accepting_transactions: true`, and `pipeline.components` showing all registered components with their states.
