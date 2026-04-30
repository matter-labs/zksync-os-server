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

All metrics are prefixed `pipeline_`. See [`src/metrics.rs`](src/metrics.rs) for
the authoritative list with descriptions.
