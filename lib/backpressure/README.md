# zksync_os_backpressure

Pipeline-aware transaction acceptance throttling for zksync-os-server.

The monitor suspends transaction acceptance when a downstream pipeline component
falls too far behind its upstream neighbour, preventing unbounded memory growth
and head-of-line blocking while the node is catching up.

---

## How backpressure is measured

Every pipeline component owns a `ComponentStateReporter` that publishes a
`ComponentState` watch channel. Each update carries:

- `block_processed` / `block_picked` — block number (and optional L2 timestamp)
  of the last item fully processed or dequeued.
- `batch_processed` / `batch_picked` — batch-number equivalent for batch-pipeline
  stages.
- `in_flight_first_batch` / `in_flight_last_batch` — for components that hold
  multiple batches concurrently (L1 senders, external job managers).

Components record these watermarks via two helpers:

- `sender.send_and_record(item, &reporter)` — records `block_processed` after the
  result is forwarded downstream.
- `receiver.recv_and_record_picked(&reporter)` — records `block_picked` at
  dequeue time, before any work begins.

A `PipelineTracker` task merges all per-component watch streams into a single
`PipelineSnapshot` (ordered list of `(ComponentId, ComponentState)` pairs).

`BackpressureMonitor` consumes the snapshot and evaluates an **adjacency window**:
it slides a two-element window over the pipeline-ordered list, skipping excluded
components (see below), and computes for each adjacent pair:

| Signal | Formula | Used for |
|---|---|---|
| `block_diff` | `upstream.block_processed − downstream.(block_processed \| block_picked)` | Block-pipeline stages |
| `time_diff` | `upstream.block_timestamp − downstream.block_timestamp` | Optional wall-clock lag |
| `batch_diff` | `upstream.batch_processed − downstream.(batch_processed \| batch_picked)` | Batch-pipeline stages |

For the downstream coordinate, `block_processed` / `batch_processed` is preferred. The `picked` fallback only applies before the very first item is fully processed — once any item completes, `block_processed` is set and never cleared (high-watermark), so the fallback is never used again after that point. A downstream component with neither set (nothing received yet) is skipped entirely.

If any component's diff **strictly exceeds** its configured threshold it is
marked as an active backpressure cause and
`TransactionAcceptanceState::NotAccepting(PipelineBackpressure { causes })` is
published. When all diffs fall back within threshold the state reverts to
`Accepting`. Transitions are logged at `WARN` / `INFO` respectively.

### Adjacency-window exclusions

Some components are deliberately skipped when computing adjacent pairs:

- **`FriJobManager` / `SnarkJobManager`** — external provers introduce inherent
  reordering; their downstream consumers (`GaplessCommitter`,
  `GaplessL1ProofSender`) already reflect the correct settled-batch watermark and
  are measured directly against the batch-pipeline upstream.
- **Pipeline sources** (`ConsensusNodeCommandSource`,
  `ExternalNodeCommandSource`) — no upstream to compare against.
- **Pipeline sinks** (`BatchSink`, `NoopSink`) — no downstream.
- **`BatchVerificationResponder`** — conditional stage (`pipe_if`) that may be
  replaced by a `NoopSink` based on config, which shifts all window pairs; it
  also only reports block numbers, making batch-diff comparisons undefined.

---

## Configuration

Thresholds are set via the top-level `[backpressure]` config section. All fields
are optional and expressed in **batch units**:

```yaml
backpressure:
  fri_prover: 100         # gapless_committer vs batch_verification
  snark_prover: 100       # gapless_l1_proof_sender vs gapless_committer
  batch_verification: 100
  upgrade_gatekeeper: 100 # default: 100 even if section is absent
  l1_senders: 100         # applies to l1_sender_commit, l1_sender_prove, l1_sender_execute
```

### Built-in defaults (no explicit config required)

| Category | Default threshold | Signal |
|---|---|---|
| Block-pipeline stages (`BlockCanonizer`, `BlockApplier`, `TreeManager`, `ProverInputGenerator`, `Batcher`, `RevmConsistencyChecker`) | 100 blocks | `block_diff_to_upstream` |
| Batch-pipeline stages (`BatchVerification`, `GaplessCommitter`, `UpgradeGatekeeper`, `L1SenderCommit/Prove/Execute`, `GaplessL1ProofSender`, `PriorityTree`) | 1000 batches | `batch_diff_to_upstream` |
| Pipeline sources / sinks | none | — |

A `PipelineCondition` can also carry `max_time_diff_to_upstream` (wall-clock lag)
as an additional or standalone signal; this is not yet exposed in the YAML config
but can be set programmatically via `BackpressureConfig::set`.

---

## Architecture

```
  ComponentStateReporter (per component)
         │ watch::Sender<ComponentState>
         ▼
  PipelineTracker (merge task)
         │ watch::Sender<PipelineSnapshot>
         ▼
  BackpressureMonitor (evaluate task)
         │ watch::Sender<TransactionAcceptanceState>
         ▼
  TxAcceptanceGate   ◄── BlockProductionDisabled (existing signal)
         │ watch::Receiver<TransactionAcceptanceState>
         ▼
  RPC server (tx acceptance check)
```

`TxAcceptanceGate` merges any number of `TransactionAcceptanceState` sources.
All `NotAccepting` reasons from every registered source are gathered and
re-emitted as a single combined state. Adding a new acceptance signal requires
only one `gate.register(rx)` call — no other logic changes.

---

## Metrics

All metrics are prefixed `pipeline_`:

| Metric | Description |
|---|---|
| `backpressure_active{component}` | 1 if this component is currently an active cause |
| `accepting` | 1 if the monitor is accepting, 0 if suspended |
| `acceptance_state_changes` | Counter: Accepting → NotAccepting transitions |
| `acceptance_state_clears` | Counter: NotAccepting → Accepting transitions |
| `component_block_diff_to_upstream{component}` | Blocks behind upstream neighbour |
| `component_batch_diff_to_upstream{component}` | Batches behind upstream neighbour |
| `component_time_diff_to_upstream_seconds{component}` | Timestamp lag vs upstream |
| `component_block_diff_to_head{component}` | Blocks behind pipeline head |
| `component_last_processed_block{component}` | Last processed block number |
| `component_last_processed_batch{component}` | Last processed batch number |
| `component_last_picked_batch{component}` | Last dequeued batch number |
| `in_flight_first_batch{component}` | Oldest in-flight batch (L1 senders, job managers) |
| `in_flight_last_batch{component}` | Newest in-flight batch |
| `in_flight_batch_count{component}` | Size of the in-flight window |
| `backpressure_threshold_block_diff_to_upstream{component}` | Configured block threshold (emitted once at startup) |
| `backpressure_threshold_batch_diff_to_upstream{component}` | Configured batch threshold (emitted once at startup) |
| `component_order{component}` | Pipeline registration order (0 = head) |

Thresholds are emitted once at startup so Grafana dashboards can show
"configured vs actual" without hard-coding a component list that drifts as the
pipeline evolves.
