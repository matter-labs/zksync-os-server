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
| `block_diff` | `upstream.block_processed − downstream.block_processed` | Block-pipeline stages |
| `time_diff` | `upstream.block_timestamp − downstream.block_timestamp` | Optional wall-clock lag |
| `batch_diff` | `upstream.batch_processed − downstream.batch_processed` | Batch-pipeline stages |

On the first `record_picked` call, `processed` is seeded to `picked − 1` so the
monitor sees a non-zero diff immediately for components that have received work
but not yet forwarded anything downstream (e.g. prover job managers when provers
are disabled). A component whose `processed` is still `None` (nothing received
yet) is skipped entirely.

If any component's diff **strictly exceeds** its configured threshold it is
marked as an active backpressure cause and
`TransactionAcceptanceState::NotAccepting(PipelineBackpressure { causes })` is
published. When all diffs fall back within threshold the state reverts to
`Accepting`. Transitions are logged at `WARN` / `INFO` respectively.

### Adjacency-window exclusions

Some components are deliberately skipped when computing adjacent pairs:

- **Pipeline sources** (`ConsensusNodeCommandSource`,
  `ExternalNodeCommandSource`) — no upstream to compare against.
- **Pipeline sinks** (`BatchSink`, `NoopSink`) — no downstream.
- **`BatchVerificationResponder`** — conditional stage (`pipe_if`) that may be
  replaced by a `NoopSink` based on config, which shifts all window pairs; it
  also only reports block numbers, making batch-diff comparisons undefined.

---

## Configuration

Thresholds are distributed to their respective component config sections. All
fields are optional and expressed in **batch units**. When unset the built-in
defaults apply (see table below).

```yaml
# Global defaults for all block- and batch-pipeline components.
# These can be raised or lowered without touching per-component overrides.
backpressure:
  default_block_diff_limit: 256   # blocks; applies to block-pipeline stages
  default_batch_diff_limit: 128   # batches; applies to batch-pipeline stages

prover_api:
  # Applied to both fri_job_manager and snark_job_manager.
  max_batch_diff_to_upstream: 100

batch_verification:
  max_batch_diff_to_upstream: 100

l1_sender:
  # Applied to l1_sender_commit, l1_sender_prove, l1_sender_execute,
  # and upgrade_gatekeeper.
  max_batch_diff_to_upstream: 100
```

Per-component overrides (e.g. `prover_api.max_batch_diff_to_upstream`) take
precedence over the global defaults.

### Built-in defaults (no explicit config required)

| Category | Default threshold | Signal |
|---|---|---|
| Block-pipeline stages (`BlockCanonizer`, `BlockApplier`, `TreeManager`, `ProverInputGenerator`, `RevmConsistencyChecker`) | 256 blocks | `block_diff_to_upstream` |
| Batch-pipeline stages (`BatchVerification`, `FriJobManager`, `SnarkJobManager`, `GaplessCommitter`, `UpgradeGatekeeper`, `L1SenderCommit/Prove/Execute`, `GaplessL1ProofSender`, `PriorityTree`) | 128 batches | `batch_diff_to_upstream` |
| `Batcher` | none — see note below | — |
| Pipeline sources / sinks | none | — |

> **Why `Batcher` has no block-diff threshold:** the Batcher's `processed`
> watermark advances per *batch*, not per block, so its `block_diff_to_upstream`
> grows naturally to O(blocks-per-batch) during every accumulation cycle.
> A block-diff threshold would either fire spuriously or need an arbitrary large
> value. Genuine Batcher stalls are caught by the downstream batch-pipeline
> components via their `batch_diff_to_upstream` thresholds.

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
