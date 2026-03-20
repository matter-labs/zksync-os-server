# Pipeline Health Monitor Design

**Date:** 2026-03-18
**Status:** Approved for implementation

## Problem

When a downstream pipeline component (FRI/SNARK prover, L1 sender, batch verifier, signing quorum) becomes slow or unresponsive, the server has no configurable, per-component policy for how to respond. The current implicit backpressure — bounded `mpsc` channels stalling upstream tasks — eventually freezes block production and stops RPC transaction acceptance, but with no operator visibility into *which* component caused it or *why*.

The existing branch (`feat/flink-pipeline-monitor`) addresses this partially but has structural problems: a global singleton that prevents test isolation, a `try_send` drop issue that can leave stale backpressure causes permanently set, only time-based conditions (no block-count dimension), two parallel acceptance-state channels in the RPC layer, and hardcoded `retry_after_ms`.

This design replaces that approach from scratch.

---

## Goals

- Per-component configurable conditions: **time in `WaitingSend`** and/or **block lag behind pipeline head**
- Single `TransactionAcceptanceState` channel owned by the monitor — no parallel channels in RPC
- Clear, structured error responses for RPC callers listing all active causes
- Prometheus gauges exposing backpressure state for operator dashboards and alerting
- No global singletons — all dependencies injected at construction
- No message drops possible — `watch::Sender` instead of `mpsc` + `try_send`
- Correct idle/stuck distinction: lag conditions only trigger when component is in `WaitingSend`

### Out of scope

- Runtime config updates (static config at startup is sufficient)
- Automatic threshold adaptation
- Unbounded channels (bounded channels remain; the monitor is an orthogonal control layer)
- Dead component detection (the existing pipeline `JoinSet` watchdog handles task crashes)

---

## Component taxonomy

Pipeline components fall into two categories with different state machine patterns:

**Pipeline-loop components** follow the `WaitingRecv → Processing → WaitingSend` cycle. They block in `WaitingSend` until the downstream channel accepts the result. Both `max_waiting_send_duration` and `max_block_lag` conditions apply and are gated on the component being in `WaitingSend`.

**Reactive components** (`FriJobManager`) use `ProcessingOrWaitingRecv` as their ambient state. They attempt a downstream send and immediately return an error if the channel is full (`try_reserve` / `TrySendError::Full`), rather than blocking in `WaitingSend`. For these components, `max_waiting_send_duration` is not applicable; only `max_block_lag` applies, and it evaluates regardless of current state.

`SnarkJobManager` is **not** reactive — it does a blocking `.await` on `prove_batches_sender.send()` and explicitly enters `WaitingSend` before the send. It is a pipeline-loop component and both conditions apply.

This distinction is documented per-component in `PipelineHealthConfig` and is not configurable at runtime — it is a fixed property of each component's implementation.

---

## Architecture

```
BlockExecutor  ──watch::Sender<ComponentHealth>──┐
BlockCanonizer ──watch::Sender<ComponentHealth>──┤
BlockApplier   ──watch::Sender<ComponentHealth>──┤
TreeManager    ──watch::Sender<ComponentHealth>──┤
     ...                                         ├──► PipelineHealthMonitor
L1Sender(Exec) ──watch::Sender<ComponentHealth>──┘         │
                                                            ├── watch::Sender<TransactionAcceptanceState>
                                                            │       │
                                                            │       └──► RPC TxHandler (single check)
                                                            │
                                                            └── Prometheus gauges
                                                                  pipeline_backpressure_active{component}
                                                                  pipeline_component_block_lag{component}
                                                                  pipeline_component_waiting_send_seconds{component}
```

Every pipeline component — including `BlockExecutor` — holds only a `ComponentHealthReporter` and hands its `watch::Receiver<ComponentHealth>` to the monitor at wiring time. No component is special-cased.

The monitor is the sole writer to `watch::Sender<TransactionAcceptanceState>` for backpressure conditions. `BlockExecutor` retains its own `watch::Sender<TransactionAcceptanceState>` exclusively for `BlockProductionDisabled` — an unrelated operational mechanism (`max_blocks_to_produce`) that hard-halts block production via `pending()` and is out of scope for this design.

---

## Section 1: Core Data Model

### `ComponentHealth`

What each pipeline stage reports on every state transition:

```rust
pub struct ComponentHealth {
    /// Current processing state.
    pub state: GenericComponentState,
    /// When the current state was entered.
    pub state_entered_at: Instant,
    /// Block number of the last item successfully sent downstream.
    ///
    /// For block-level components (BlockExecutor, BlockApplier, TreeManager,
    /// ProverInputGenerator): this is the block number directly.
    ///
    /// For batch-level components (Batcher, FriJobManager, L1Sender, etc.):
    /// this is the block number of the highest block included in the last
    /// processed batch. This keeps the sequence space uniform — all components
    /// use block numbers, enabling a single lag comparison against the pipeline head.
    ///
    /// Updated by calling `record_processed(seq)` after output.send() returns.
    pub last_processed_seq: u64,
}
```

Using block numbers uniformly across all components means `max_block_lag` has consistent semantics: "this component's work has not advanced past block N, while the pipeline head is at N+lag." For a Batcher that last batched up to block 1000 with head at 1050, the lag is 50 blocks — a meaningful operator-visible number regardless of how many batches that represents.

### `BackpressureCondition`

Per-component configuration. Both fields are optional; either or both can be set. All default to `None`, making the feature fully opt-in.

```rust
#[derive(Default, Clone)]
pub struct BackpressureCondition {
    /// Trigger if component stays in WaitingSend longer than this duration.
    /// Not applicable to reactive components (FriJobManager only).
    pub max_waiting_send_duration: Option<Duration>,

    /// Trigger if component is more than N blocks behind the pipeline head.
    /// For pipeline-loop components: only evaluated when in WaitingSend.
    /// For reactive components: evaluated regardless of state.
    pub max_block_lag: Option<u64>,
}
```

### `ComponentId`

An enum identifying each component. Used to look up conditions from config and as a key for Prometheus label values.

```rust
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
            Self::BlockExecutor       => "block_executor",
            Self::BlockApplier        => "block_applier",
            Self::TreeManager         => "tree_manager",
            Self::BlockCanonizer      => "block_canonizer",
            Self::ProverInputGenerator => "prover_input_generator",
            Self::Batcher             => "batcher",
            Self::BatchVerification   => "batch_verification",
            Self::FriJobManager       => "fri_job_manager",
            Self::GaplessCommitter    => "gapless_committer",
            Self::UpgradeGatekeeper   => "upgrade_gatekeeper",
            Self::L1SenderCommit      => "l1_sender_commit",
            Self::SnarkJobManager     => "snark_job_manager",
            Self::GaplessL1ProofSender => "gapless_l1_proof_sender",
            Self::L1SenderProve       => "l1_sender_prove",
            Self::PriorityTree        => "priority_tree",
            Self::L1SenderExecute     => "l1_sender_execute",
        }
    }

    /// Whether this component is reactive (never holds WaitingSend for a measurable duration).
    /// FriJobManager uses try_reserve rather than blocking .await — it immediately returns
    /// TrySendError::Full when the channel is full rather than waiting in WaitingSend.
    /// For reactive components, max_waiting_send_duration is ignored and
    /// max_block_lag evaluates regardless of state.
    ///
    /// SnarkJobManager is NOT reactive — it does a blocking .await in WaitingSend.
    pub fn is_reactive(self) -> bool {
        matches!(self, Self::FriJobManager)
    }
}
```

### `PipelineHealthConfig`

```rust
pub struct PipelineHealthConfig {
    /// How often the monitor re-evaluates all conditions.
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
            eval_interval: Duration::from_secs(1), // explicit: Duration::default() is zero
            block_executor:           BackpressureCondition::default(),
            block_applier:            BackpressureCondition::default(),
            // ... all fields BackpressureCondition::default()
        }
    }
}

impl PipelineHealthConfig {
    pub fn condition_for(&self, id: ComponentId) -> &BackpressureCondition {
        match id {
            ComponentId::BlockExecutor       => &self.block_executor,
            ComponentId::BlockApplier        => &self.block_applier,
            ComponentId::TreeManager         => &self.tree_manager,
            ComponentId::BlockCanonizer      => &self.block_canonizer,
            ComponentId::ProverInputGenerator => &self.prover_input_generator,
            ComponentId::Batcher             => &self.batcher,
            ComponentId::BatchVerification   => &self.batch_verification,
            ComponentId::FriJobManager       => &self.fri_job_manager,
            ComponentId::GaplessCommitter    => &self.gapless_committer,
            ComponentId::UpgradeGatekeeper   => &self.upgrade_gatekeeper,
            ComponentId::L1SenderCommit      => &self.l1_sender_commit,
            ComponentId::SnarkJobManager     => &self.snark_job_manager,
            ComponentId::GaplessL1ProofSender => &self.gapless_l1_proof_sender,
            ComponentId::L1SenderProve       => &self.l1_sender_prove,
            ComponentId::PriorityTree        => &self.priority_tree,
            ComponentId::L1SenderExecute     => &self.l1_sender_execute,
        }
    }
}
```

`CommandSource` and `BatchSink` are source/sink — they have no upstream to block, so they are not included. Components absent from the current node role (e.g. `batcher` on an external node) are simply not registered with the monitor and their conditions are ignored.

---

## Section 2: `ComponentHealthReporter`

Replaces `ComponentStateReporter` entirely. Uses `watch::Sender<ComponentHealth>` so state updates are infallible — no capacity, no drops, no background task.

```rust
pub struct ComponentHealthReporter {
    sender: watch::Sender<ComponentHealth>,
    component: &'static str,
}

impl ComponentHealthReporter {
    /// Returns the reporter (for the component) and the receiver (for the monitor).
    /// No global state — safe to construct multiple instances in tests.
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentHealth>) {
        let (sender, receiver) = watch::channel(ComponentHealth {
            state: GenericComponentState::WaitingRecv,
            state_entered_at: Instant::now(),
            last_processed_seq: 0,
        });
        (Self { sender, component }, receiver)
    }

    /// Transition to a new state.
    /// Records the Prometheus time-in-state metric for the state being left,
    /// inline and atomically with the state update. No background task needed.
    pub fn enter_state(&self, new_state: GenericComponentState) {
        let now = Instant::now();
        self.sender.send_modify(|health| {
            let elapsed = now.duration_since(health.state_entered_at);
            COMPONENT_METRICS.time_in_state[&(self.component, health.state)].observe(elapsed);
            health.state = new_state;
            health.state_entered_at = now;
        });
    }

    /// Record the block number of the last item successfully sent downstream.
    /// For batch-level components, use the block number of the highest block
    /// in the processed batch.
    /// Call after output.send() returns.
    pub fn record_processed(&self, block_seq: u64) {
        self.sender.send_modify(|health| {
            health.last_processed_seq = block_seq;
        });
    }
}
```

### Migration diff per component

The call sites change minimally — one new call per loop iteration:

```rust
// Before
reporter.enter_state(GenericComponentState::WaitingSend);
output.send(result).await?;

// After
reporter.enter_state(GenericComponentState::WaitingSend);
output.send(result).await?;
reporter.record_processed(item.block_number); // ← only addition
```

### What disappears

- The background reporter task in `ComponentStateReporter`
- The 512-capacity `mpsc` channel with `try_send`
- The `tick_backpressure()` polling loop from the existing branch
- The `ComponentStateHandle` split type
- The global `BackpressureHandle::global()` singleton

### What is preserved

- `component_time_spent_in_state` Prometheus metric with identical labels — no dashboard breakage
- `GenericComponentState` enum unchanged
- `enter_state` call sites in each component — one extra line added

---

## Section 3: `PipelineHealthMonitor`

Single task. Owns the acceptance state sender. Subscribes to all component health receivers. Evaluates conditions on a configurable interval.

```rust
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
        let (acceptance_tx, acceptance_rx) =
            watch::channel(TransactionAcceptanceState::Accepting);
        (
            Self { config, components: vec![], acceptance_tx, stop_receiver },
            acceptance_rx,
        )
    }

    /// Called during pipeline wiring for each component present in this node role.
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
                    tracing::info!("PipelineHealthMonitor: stop signal received, shutting down");
                    return;
                }
            }
        }
    }
}
```

### Evaluation

```rust
fn evaluate_and_update(&self) {
    // Pipeline head = last block processed by BlockExecutor.
    // All downstream lag is measured against this.
    let head_seq = self.components
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| rx.borrow().last_processed_seq)
        .unwrap_or(0);

    let mut active_causes: Vec<BackpressureCause> = self.components
        .iter()
        .filter_map(|(id, rx)| self.evaluate(*id, &rx.borrow(), head_seq))
        .collect();

    // Deterministic ordering: stable watch value when conditions haven't changed.
    active_causes.sort_by_key(|c| c.component);

    // Emit Prometheus gauges on every tick, independent of acceptance state changes.
    self.emit_metrics(&active_causes, head_seq);

    let new_state = self.compute_acceptance_state(active_causes);

    // send_if_modified: only wakes RPC watchers when acceptance state actually changes.
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

fn compute_acceptance_state(
    &self,
    active_causes: Vec<BackpressureCause>,
) -> TransactionAcceptanceState {
    if active_causes.is_empty() {
        TransactionAcceptanceState::Accepting
    } else {
        TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes: active_causes },
        )
    }
}
```

### Condition evaluation — idle vs stuck

**For pipeline-loop components:** both conditions are gated on `WaitingSend`. This is the critical correctness property:

- `WaitingRecv` — component is idle, waiting for input. It is not stuck; it has nothing to process.
- `Processing` — actively working. Transient lag is expected.
- `WaitingSend` — component has completed work and is blocked pushing it downstream. This is real lag.

An idle sequencer with no transactions will have downstream components in `WaitingRecv` with `last_processed_seq` far behind `BlockExecutor`. This is normal and must not trigger backpressure.

**For reactive components (`FriJobManager` only):** this component never holds `WaitingSend` for a measurable duration — it uses `try_reserve` and returns immediately on `TrySendError::Full`. Only `max_block_lag` applies, evaluated regardless of state. The idle/stuck ambiguity does not arise here because `FriJobManager` processes batches, not individual empty blocks; its `last_processed_seq` (highest block in last proven batch) only lags when the prover genuinely falls behind batch production.

```rust
fn evaluate(
    &self,
    id: ComponentId,
    health: &ComponentHealth,
    head_seq: u64,
) -> Option<BackpressureCause> {
    let condition = self.config.condition_for(id);
    let now = Instant::now();

    if id.is_reactive() {
        // Reactive components: only block_lag, no state gate.
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

    // Pipeline-loop components: both conditions require WaitingSend.
    if health.state != GenericComponentState::WaitingSend {
        return None;
    }

    // Trigger 1: stuck in WaitingSend too long (takes priority if both exceeded).
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

    // Trigger 2: far behind head while blocked (not just idle).
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
```

### Prometheus metrics

Emitted on every evaluation tick regardless of whether acceptance state changed.
`pipeline_component_block_lag` and `pipeline_component_waiting_send_seconds` emit `0` when the component is not in `WaitingSend`, so idle components do not show misleading non-zero lag values.

```rust
fn emit_metrics(&self, active_causes: &[BackpressureCause], head_seq: u64) {
    for (id, rx) in &self.components {
        let health = rx.borrow();
        let is_active = active_causes.iter().any(|c| c.component == id.as_str());
        MONITOR_METRICS.backpressure_active[id].set(is_active as u64);

        // For pipeline-loop components: emit lag/secs only in WaitingSend so idle
        // components (WaitingRecv) show 0, not a misleading large number.
        // For reactive components: emit block_lag always (it's meaningful regardless
        // of state), but always emit waiting_send_seconds=0 (the metric doesn't apply
        // to them and a non-zero value would be misleading on dashboards).
        let (lag, waiting_send_secs) = if id.is_reactive() {
            (head_seq.saturating_sub(health.last_processed_seq), 0.0)
        } else if health.state == GenericComponentState::WaitingSend {
            let lag = head_seq.saturating_sub(health.last_processed_seq);
            let secs = Instant::now()
                .duration_since(health.state_entered_at)
                .as_secs_f64();
            (lag, secs)
        } else {
            (0, 0.0)
        };

        MONITOR_METRICS.block_lag[id].set(lag);
        MONITOR_METRICS.waiting_send_seconds[id].set(waiting_send_secs);
    }
}
```

New Prometheus metrics:

| Metric | Type | Description |
|---|---|---|
| `pipeline_backpressure_active{component}` | Gauge (0/1) | Whether this component is currently an active backpressure cause |
| `pipeline_component_block_lag{component}` | Gauge | Blocks behind pipeline head; 0 when idle (WaitingRecv) |
| `pipeline_component_waiting_send_seconds{component}` | Gauge | Seconds in WaitingSend; 0 when idle |

Existing `component_time_spent_in_state` histogram is preserved with identical labels.

---

## Section 4: RPC Layer and Error Format

### Single acceptance state check

`TxHandler` has one check, one channel:

```rust
pub struct TxHandler {
    /// Sole acceptance state receiver. PipelineHealthMonitor owns the sender.
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    // ... unchanged
}

async fn send_raw_transaction_impl(&self, tx: Bytes) -> Result<H256, EthSendRawTransactionError> {
    if let TransactionAcceptanceState::NotAccepting(reason) = &*self.acceptance_state.borrow() {
        return Err(EthSendRawTransactionError::NotAcceptingTransactions(reason.clone()));
    }
    // ... rest unchanged
}
```

`BlockExecutor` retains its existing `watch::Sender<TransactionAcceptanceState>` for `BlockProductionDisabled` — that mechanism is unchanged. The only change to `BlockExecutor` in this design is replacing `ComponentStateReporter` with `ComponentHealthReporter`.

### Error format

Both `NotAcceptingReason` variants return JSON-RPC code `-32003` (Transaction Rejected). `BlockProductionDisabled` previously returned `-32603` (Internal Error) — a deliberate operator config limit is not an internal error.

**`BlockProductionDisabled`:**
```json
{
    "code": -32003,
    "message": "Not accepting transactions: block production is disabled",
    "data": { "reason": "block_production_disabled" }
}
```

**`PipelineBackpressure`** — lists all active causes with threshold and actual values:
```json
{
    "code": -32003,
    "message": "Not accepting transactions: pipeline backpressure",
    "data": {
        "reason": "pipeline_backpressure",
        "causes": [
            {
                "component": "fri_job_manager",
                "trigger": "block_lag_too_high",
                "threshold_blocks": 500,
                "actual_blocks": 782
            },
            {
                "component": "l1_sender_commit",
                "trigger": "waiting_send_too_long",
                "threshold_secs": 3600,
                "actual_secs": 4215
            }
        ]
    }
}
```

No `retry_after_ms` — callers use standard exponential backoff. The `actual_secs` / `actual_blocks` fields give operators enough to understand severity.

### Extended `NotAcceptingReason`

```rust
#[derive(Clone, PartialEq)]
pub enum NotAcceptingReason {
    BlockProductionDisabled,
    PipelineBackpressure { causes: Vec<BackpressureCause> },
}

#[derive(Clone, PartialEq)]
pub struct BackpressureCause {
    pub component: &'static str,
    pub trigger: BackpressureTrigger,
}

#[derive(Clone, PartialEq)]
pub enum BackpressureTrigger {
    WaitingSendTooLong { threshold: Duration, actual: Duration },
    BlockLagTooHigh    { threshold: u64,      actual: u64 },
}
```

---

## Testing

### Unit tests — `PipelineHealthMonitor::evaluate`

Test `evaluate()` directly with constructed `ComponentHealth` values. No server or runtime needed.

```
 1. Pipeline-loop, WaitingRecv, block lag → None   (idle, not stuck)
 2. Pipeline-loop, Processing, lag         → None   (active, not stuck)
 3. Pipeline-loop, WaitingSend, duration exceeded → WaitingSendTooLong cause
 4. Pipeline-loop, WaitingSend, block lag exceeded → BlockLagTooHigh cause
 5. Pipeline-loop, WaitingSend, both exceeded → WaitingSendTooLong (takes priority)
 6. Pipeline-loop, WaitingSend, neither exceeded → None (below thresholds)
 7. Reactive (FriJobManager), any state, block lag exceeded → BlockLagTooHigh cause
 8. Reactive (FriJobManager), any state, block lag not exceeded → None
 9. Two components with active causes → both in acceptance state
10. One cause clears → other remains → still NotAccepting
11. All causes clear → Accepting
12. Lag metrics emit 0 for WaitingRecv components, non-zero for WaitingSend
```

### Integration test — end-to-end RPC rejection and recovery

`FriJobManager` is reactive; the condition to use is `max_block_lag`, not `max_waiting_send_duration`.

**What creates the lag:** `FriJobManager.last_processed_seq` starts at 0 and only advances when a proof is submitted downstream. `BlockExecutor.last_processed_seq` advances on every block (including empty ones at 4/sec). Setting `max_block_lag: Some(1)` means backpressure triggers as soon as `BlockExecutor` processes at least 2 blocks while `FriJobManager` has processed zero — guaranteed quickly at startup with any non-zero block production. The `fake_fri_provers.compute_time` delay ensures the lag persists long enough to assert on.

Recovery is verified by polling until `eth_sendRawTransaction` succeeds (after the prover finishes), rather than using a fixed sleep — the fixed-sleep approach is fragile and tied to compute_time tuning.

```rust
#[tokio::test]
async fn backpressure_stops_and_resumes_transaction_acceptance() {
    let node = TestNode::start(NodeConfig {
        fake_fri_provers: FakeProverConfig {
            compute_time: Duration::from_secs(10), // prover is slow
            ..
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
    }).await;

    // Submit enough transactions to trigger a batch and push it to FriJobManager.
    node.submit_transactions(50).await;

    // Wait for the monitor to detect the lag (eval_interval * 2 for safety).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // RPC must reject with -32003.
    let err = node.send_raw_transaction(dummy_tx()).await.unwrap_err();
    assert_eq!(err.code, -32003);
    assert_eq!(err.data["reason"], "pipeline_backpressure");
    assert_eq!(err.data["causes"][0]["component"], "fri_job_manager");
    assert_eq!(err.data["causes"][0]["trigger"], "block_lag_too_high");

    // Poll until the prover finishes and lag clears (avoids fixed-sleep fragility).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for backpressure to clear");
        if node.send_raw_transaction(dummy_tx()).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

---

## Pipeline wiring changes summary

| Component | Before | After |
|---|---|---|
| Each pipeline component | `ComponentStateReporter` + background task | `ComponentHealthReporter` (watch-based, no background task) |
| `BlockExecutor` | Holds `watch::Sender<TransactionAcceptanceState>` + `ComponentStateReporter` | Holds `watch::Sender<TransactionAcceptanceState>` (unchanged) + `ComponentHealthReporter` (replaces `ComponentStateReporter`) |
| `TxHandler` | Two sequential acceptance checks | One check, one channel |
| `BackpressureHandle` (branch) | Global singleton, `OnceLock` | Removed entirely |
| Acceptance state sender | Owned by `BlockExecutor` | Owned by `PipelineHealthMonitor` |
| New: `PipelineHealthMonitor` | — | Spawned as pipeline task; all receivers registered at wiring time; participates in the node's stop signal |
| `run_status_server` / `AppState` | Only stop signal; returns `{ healthy }` | Extended with acceptance state + component health receivers; returns full pipeline snapshot |

---

## Section 5: `/status/health` Extension

The existing status server at `lib/status/` already serves `GET /status/health` via axum on its own port (`StatusServerConfig.address`). Currently it returns only `{ "healthy": bool }` based on the stop signal.

### `AppState` extension

`AppState` gains two new fields passed in at startup wiring:

```rust
#[derive(Clone)]
struct AppState {
    stop_receiver: watch::Receiver<bool>,
    /// From PipelineHealthMonitor.
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    /// One entry per registered component, same set as the monitor.
    component_health: Arc<Vec<(ComponentId, watch::Receiver<ComponentHealth>)>>,
}
```

### Response format

HTTP `200 OK` when healthy, `503 Service Unavailable` when terminating or not accepting transactions — preserving the existing semantics that load balancer probes rely on.

```rust
#[derive(Serialize)]
struct HealthResponse {
    healthy: bool,
    accepting_transactions: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    backpressure_causes: Vec<BackpressureCauseJson>,
    pipeline: PipelineSnapshot,
}

#[derive(Serialize)]
struct PipelineSnapshot {
    head_block: u64,
    components: IndexMap<&'static str, ComponentSnapshot>, // ordered by pipeline position
}

#[derive(Serialize)]
struct ComponentSnapshot {
    state: &'static str,           // "waiting_recv" | "processing" | "waiting_send" | ...
    state_duration_secs: f64,
    last_processed_block: u64,
    block_lag: u64,                // 0 when idle (WaitingRecv)
    waiting_send_secs: f64,        // 0 when not in WaitingSend
}

#[derive(Serialize)]
struct BackpressureCauseJson {
    component: &'static str,
    trigger: &'static str,         // "waiting_send_too_long" | "block_lag_too_high"
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_blocks: Option<u64>,
}
```

### Example responses

**Healthy node:**
```json
HTTP 200
{
  "healthy": true,
  "accepting_transactions": true,
  "pipeline": {
    "head_block": 12345,
    "components": {
      "block_executor":   { "state": "processing",              "state_duration_secs": 0.04, "last_processed_block": 12344, "block_lag": 0,   "waiting_send_secs": 0 },
      "block_canonizer":  { "state": "waiting_recv",            "state_duration_secs": 0.21, "last_processed_block": 12344, "block_lag": 0,   "waiting_send_secs": 0 },
      "fri_job_manager":  { "state": "processing_or_waiting_recv", "state_duration_secs": 1.2, "last_processed_block": 12300, "block_lag": 45, "waiting_send_secs": 0 },
      "l1_sender_commit": { "state": "waiting_recv",            "state_duration_secs": 0.8,  "last_processed_block": 12290, "block_lag": 0,   "waiting_send_secs": 0 }
    }
  }
}
```

**Node under backpressure:**
```json
HTTP 503
{
  "healthy": false,
  "accepting_transactions": false,
  "backpressure_causes": [
    { "component": "fri_job_manager", "trigger": "block_lag_too_high", "threshold_blocks": 500, "actual_blocks": 782 },
    { "component": "l1_sender_commit", "trigger": "waiting_send_too_long", "threshold_secs": 3600, "actual_secs": 4215 }
  ],
  "pipeline": {
    "head_block": 15000,
    "components": {
      "block_executor":   { "state": "waiting_send",  "state_duration_secs": 12.3, "last_processed_block": 15000, "block_lag": 0,   "waiting_send_secs": 12.3 },
      "fri_job_manager":  { "state": "processing_or_waiting_recv", "state_duration_secs": 900.1, "last_processed_block": 14218, "block_lag": 782, "waiting_send_secs": 0 },
      "l1_sender_commit": { "state": "waiting_send",  "state_duration_secs": 4215, "last_processed_block": 14880, "block_lag": 120, "waiting_send_secs": 4215 }
    }
  }
}
```

### Handler

```rust
pub(crate) async fn health(
    state: axum::extract::State<AppState>,
) -> (StatusCode, Json<HealthResponse>) {
    let is_terminating = *state.stop_receiver.borrow();
    let acceptance = state.acceptance_state.borrow().clone();
    let accepting = matches!(acceptance, TransactionAcceptanceState::Accepting);

    let head_block = state.component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| rx.borrow().last_processed_seq)
        .unwrap_or(0);

    let components = state.component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            let elapsed = h.state_entered_at.elapsed().as_secs_f64();
            let lag = head_block.saturating_sub(h.last_processed_seq);
            let waiting_send_secs = if h.state == GenericComponentState::WaitingSend {
                elapsed
            } else {
                0.0
            };
            (id.as_str(), ComponentSnapshot {
                state: h.state.as_str(),
                state_duration_secs: elapsed,
                last_processed_block: h.last_processed_seq,
                block_lag: if h.state == GenericComponentState::WaitingSend || id.is_reactive() { lag } else { 0 },
                waiting_send_secs,
            })
        })
        .collect();

    let backpressure_causes = match &acceptance {
        TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes }
        ) => causes.iter().map(BackpressureCauseJson::from).collect(),
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

### What changes in `lib/status/`

- `AppState` gains `acceptance_state` and `component_health` fields
- `run_status_server` signature gains those two parameters
- `health.rs` replaced with the handler above
- No new routes, no new ports, no new config
