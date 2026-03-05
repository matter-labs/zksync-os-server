# AI Code Review Guide for zksync-os-server

*This guide covers the conventions, idioms, and correctness expectations that apply to zksync-os-server.*

---

## Page 1: Architecture & Component Model

### 1.1 Pipeline Component Discipline

This codebase uses a custom pipeline framework (`lib/pipeline/`). Components implement `PipelineComponent` and are chained via `Pipeline::pipe()`. Tasks are collected into a `JoinSet<()>` in `run()`.

**What to flag:**
- `tokio::spawn` in library crates — background work must go through the pipeline framework or be explicitly registered into the `JoinSet` in `node/bin/src/lib.rs`
- A `PipelineComponent` that does not honour channel closure: `input.recv()` returning `None` is the shutdown signal for pipeline components — exit cleanly, do not loop indefinitely
- Side-tasks spawned inside a `PipelineComponent::run()` without propagating their errors back (e.g., fire-and-forget `tokio::spawn` inside a component)
- `todo!()` / `unimplemented!()` left in production paths after a refactor

**Correct pattern:**
```rust
#[async_trait]
impl PipelineComponent for MyComponent {
    type Input = BlockCommand;
    type Output = ProcessedBlock;
    const NAME: &'static str = "my_component";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        while let Some(cmd) = input.recv().await {
            let result = self.process(cmd).await?;
            if output.send(result).await.is_err() {
                // downstream closed — clean exit
                break;
            }
        }
        Ok(())
    }
}
```

Non-pipeline background tasks (watchers, RPC servers, Prometheus) must be spawned directly into the `JoinSet` in `lib.rs` and receive a `stop_receiver: watch::Receiver<bool>`.

### 1.2 Cancellation & Shutdown Awareness

All long-running non-pipeline tasks must accept and honour a `stop_receiver: watch::Receiver<bool>`.

**What to flag:**
- A background loop that polls or sleeps without ever checking `stop_receiver`
- Using `.borrow()` to check the stop signal without also awaiting `.changed()` — the borrow only reflects the current value; you need both
- Dangling tasks explicitly noted in the code (e.g., `// todo: dangling task` comments in `eth_pubsub_impl.rs` and `eth_filter_impl.rs`) — flag these and request a proper registration

**Correct pattern:**
```rust
pub async fn run(mut self, mut stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(self.poll_interval);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                self.do_work().await?;
            }
            _ = stop_receiver.changed() => {
                tracing::debug!("MyComponent received stop signal, shutting down");
                return Ok(());
            }
        }
    }
}
```

### 1.3 Configuration System (`smart-config`)

Config uses the `smart-config` crate with `DescribeConfig`/`DeserializeConfig` derive macros. Fields are exposed as environment variables (underscore-separated path prefix + field name).

**What to flag:**
- Struct fields that are meaningless at their zero/empty defaults — prefer `Option<SubConfig>` rather than `url: String` defaulting to `""` or `Duration::ZERO`
- Missing `#[config(default_t = ...)]` where omission would panic on a standard deployment — verify against `local-chains/` config files
- Scattered flat fields that belong together — introduce a named sub-struct and use `#[config(flatten)]`
- `#[config(derive(Default))]` on a config where the all-default state is invalid for production — either add validation or use `Option`
- Renaming a field changes the environment variable key — always a breaking change even in internal PRs
- Adding required fields (no `default_t`) to an existing deployed config struct — that's a breaking change for operators

**Correct pattern:**
```rust
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct PollingConfig {
    /// How often to poll L1 for new events.
    #[config(default_t = Duration::from_secs(5), with = TimeUnit::Seconds)]
    pub poll_interval: Duration,
    /// Max L1 blocks to inspect per poll.
    #[config(default_t = 100)]
    pub max_blocks_per_poll: u64,
}

// — not —
// pub poll_interval_secs: u64,   // ambiguous unit
// pub max_blocks: u64,           // missing default, breaks deployment
```

### 1.4 Historical VM Versioning

The codebase maintains multiple historical versions of `forward_system` (e.g., `zk_os_forward_system_0_0_28`, `zk_os_forward_system_0_1_2`) to replay old blocks, alongside a current version used to produce new ones.

**What to flag:**
- Changing the `zksync_os_interface` version without back-porting to all historical versions — the workspace `Cargo.toml` documents this requirement explicitly
- Moving a version from "current" to "historical" without removing the now-unneeded `zk_ee` / `zk_os_basic_system` dependencies for that version
- Feature flag changes on a version already used in production block history — those blocks must still be replayable
- Using the `dev` dependency (`zk_os_forward_system_dev`) in code paths that run on mainnet data — dev versions are for local experiments only

### 1.5 API Surface Minimalism

Before adding a new `ReadRepository` / `WriteState` / `ReadReplay` trait method, verify the data isn't already accessible through existing methods. Every new trait method is a permanent obligation across all implementations (`db`, `in_memory`, `lazy`).

- Compose existing APIs where possible
- `macro_rules!` used to reduce duplication in method bodies should be replaced with a private helper function
- Unused `pub` methods on storage types should be removed or demoted to `pub(crate)`

---

## Page 2: Rust Idioms & Code Quality

### 2.1 Type System Discipline

**Use domain types over raw primitives.** The codebase defines semantic wrappers — use them:
- `L1TxSerialId` not `u64` for L1 priority transaction serial IDs
- `ProtocolSemanticVersion` / `ProvingVersion` / `ExecutionVersion` not bare version integers
- `NodeRole` not a raw bool for main-node vs external-node branching
- Alloy's `Address`, `BlockHash`, `TxHash`, `BlockNumber` from `alloy::primitives` — not `[u8; 20]` or `u64` directly

**Avoid `unwrap_or_default()` where `None` has domain meaning.** In this codebase, an absent `contract_address` in a transaction means EVM contract deployment — silently substituting `Address::ZERO` is semantically wrong.

**`#[derive(Default)]` is wrong when the zero state is an error state.** A `ReplayRecord` with default fields or a mock result with `success: false` should not implement `Default`. Use a named constructor instead.

**Alloy types have specific encoding expectations** — prefer the provided encoding methods (`Encodable2718`, `Decodable2718`, RLP) over manual byte manipulation.

### 2.2 Error Handling & Conversions

- `.context("...")` over `.ok_or_else(|| anyhow::anyhow!("..."))`
- `.context(...)` over `.unwrap()` for any `Option`/`Result` that can fail in production
- Return `anyhow::Result` from fallible operations; panics are appropriate only for unrecoverable startup failures
- Retries on transient errors must use `backon` (in workspace deps) — not a raw `loop { sleep; retry }` pattern
- Saturating arithmetic for block numbers: `block.saturating_sub(n)` instead of `block - n` which panics on underflow
- RocksDB access errors should propagate with context, not be swallowed: `.context("reading batch header")?` not `.unwrap()`

### 2.3 Cloning & Borrowing

Every redundant clone is worth flagging:
- `value.clone()` before a `match` → `match &value`
- `arc.clone()` inside a tight loop for each item → clone once outside the loop
- `vec.as_slice()` passed by reference → `&vec`
- Unnecessary `.collect::<Vec<_>>()` before an iteration that doesn't need ownership

`Arc<RwLock<T>>` is expensive under write contention. Flag new uses that will be written frequently. Consider `tokio::sync::watch` for single-writer / multi-reader state updates instead.

### 2.4 Iterator & Collection Idioms

- `for (i, x) in iter.enumerate()` over `.enumerate().for_each(|(i, x)| ...)`
- `filter_map(...).collect()` over a `mut vec` with conditional `push`
- `extend([(a, b)])` over `extend(vec![(a, b)])`
- Don't manually bound a `zip` — `zip` already stops at the shorter iterator
- Prefer `BTreeMap` over `HashMap` when iteration order matters for determinism (e.g., in batch proof inputs)

### 2.5 Control Flow & Pattern Matching

- No `matches!(x, A) { match x { A => ..., _ => unreachable!() } }` double-matching
- `_ => unreachable!()` on enums that may gain variants in future protocol upgrades — enumerate exhaustively or fail loudly with context
- Prefer `match` over `if let` chains for multi-variant enums
- Inline single-use temporaries in format strings: `tracing::info!("block {block_number}")` not `let s = block_num.to_string(); tracing::info!("{s}")`

### 2.6 Naming Conventions

| Anti-pattern | Preferred |
|---|---|
| `_ref_mut` suffix | `_mut` suffix |
| `_folder` | `_dir` or `_directory` |
| `_Inner` on a public type | Rename to reflect role (`_Builder`, `_Handle`, `_Initialized`) |
| `unwrap_or_default()` where default is semantically invalid | `Option` + explicit handling |
| `SomeType::default()` where zero is meaningful | `SomeType(0)` for transparency |
| Importing enum variant names directly | Import only the enum type |

**Names must match semantics.** Instrumentation span names must match the actual method name. A method that resets something should not be named `return_to_*`.

---

## Page 3: Correctness, Storage & Testing

### 3.1 RocksDB Usage

This codebase uses RocksDB via column families defined with `NamedColumnFamily`. RocksDB handles are cheap to clone; there are no connection pools or SQL transaction isolation levels.

**What to flag:**
- Operations that must be atomic but are split across two separate `WriteBatch` commits — use a single batch
- Reading a value from one column family and conditionally writing to another without a batch — another writer can interleave
- Missing `flush()` before considering data durable
- Growing unbounded scans: iterating all keys in a CF that grows without bound — add a per-block compaction or store an index pointer
- Reading from RocksDB on every loop iteration when the value changes rarely — consider caching with invalidation via `watch::Receiver`

**Crash safety:** This codebase uses a WAL-based replay model (`block_replay_storage.rs`). For every pair of sequential non-atomic operations, the author must reason through the recovery path: *"step B is idempotent"*, *"the WAL allows replay to reconstruct B"*, or *"we accept a bounded inconsistency window (documented)"*.

### 3.2 Concurrency & State Sharing

- `Arc<RwLock<T>>` for shared mutable state — flag any `write()` lock held across an `await` point
- `tokio::sync::watch` for single-writer broadcast — prefer over `Arc<RwLock<T>>` when only the latest value matters
- `DashMap` (in workspace deps) for concurrent hash maps — use it instead of `Mutex<HashMap<K,V>>`
- Channel backpressure: `mpsc::channel` with a small buffer is the intended backpressure mechanism between pipeline stages — don't increase `OUTPUT_BUFFER_SIZE` without understanding the downstream latency implications

### 3.3 Domain Correctness Questions

Before accepting an implementation, probe:
- Does this filter/check apply to L1 transactions, L2 transactions, or both? The distinction matters for mempool handling.
- Can this value (L1 block number, gas price, base token ratio) change between block production and batch commitment? If yes, does the code snapshot it at the right point?
- Is a RocksDB read on every block production iteration actually necessary, or can it be loaded once and cached?
- Does the new code correctly handle the `ExternalNode` role vs `MainNode` role? Some code paths diverge on `NodeRole::is_main()`.
- For VM version branching (`multivm`): does the new code apply to all supported versions, including historical ones used for replay?
- Are block numbers compared correctly? `BlockNumber` (u64) arithmetic must be saturating or explicitly range-checked.

### 3.4 Performance Awareness

- **N+1 RocksDB reads in loops.** Loading one field per block in a loop when a range scan would work — prefer batch reads.
- **Large data copies.** `BlockOutput` and replay records can be large — prefer `Arc<T>` or passing by channel rather than cloning across boundaries.
- **Prover input generation** is CPU-intensive — must not block the Tokio runtime. Use `tokio::task::spawn_blocking` or `rayon` (in workspace deps) for CPU-bound work.
- **Merkle tree operations** are I/O-bound — hold locks only for the minimum duration; do not hold the tree lock across a network call.

### 3.5 Testing Standards

**Tests must assert something.** `println!("{:#?}", result)` without `assert_eq!` / `assert!(...)` is not a test.

**Use `insta` for snapshot tests** (in workspace deps) for complex structured outputs — but only where the structure is stable. Avoid snapshot tests for data that includes timestamps, hashes of random keys, or other non-deterministic values.

**Parameterize with explicit cases, not opaque indices.** A failing test that reports `case 3 failed` is harder to debug than one that reports the actual input values.

**Test doubles for storage traits** (`ReadRepository`, `WriteState`, etc.) should use semantically valid initial states. A mock `ReadStateHistory` that returns an empty-but-valid genesis block is fine; one that returns zeroed-out memory is not.

**Integration tests** (`integration-tests/`) use a full node instance. Changes to startup order, config parsing, or pipeline assembly in `lib.rs` must be validated by running integration tests, not just unit tests.

**Avoid `unwrap()` in tests for paths that can legitimately fail** — use `?` in a `#[tokio::test]` returning `anyhow::Result<()>` to get clear failure messages.

---

## Page 4: Observability, Documentation & Review Process

### 4.1 Metrics Standards

Metrics use the `vise` crate. The pattern is:

```rust
#[derive(Debug, Metrics)]
pub struct MyMetrics {
    #[metrics(labels = ["operation"])]
    pub duration: LabeledFamily<&'static str, Histogram<Duration>>,
    pub items_processed: Counter,
}

#[vise::register]
pub static METRICS: vise::Global<MyMetrics> = vise::Global::new();
```

**What to flag:**
- Metrics defined but never incremented/observed
- Using `Counter` for a value that can decrease — use `Gauge`
- Label cardinality explosion: don't label by block number, tx hash, or any unbounded value
- Missing metrics for a new component that runs in production (latency, error count, and items processed are the minimum)

### 4.2 Logging Standards

| Situation | Level |
|---|---|
| Periodic routine success (e.g., "fetched N L1 events") | `TRACE` |
| Unusual but recoverable condition | `WARN` |
| Error requiring operator attention | `ERROR` |
| Per-block / per-tx debug data | `DEBUG` |
| Task shutdown / clean exit | `DEBUG` |
| Node startup / major state transitions | `INFO` |

**Structured fields, not string interpolation.** Prefer:
```rust
tracing::info!(block_number, tx_count, "executed block");
// — not —
tracing::info!("executed block #{} with {} txs", block_number, tx_count);
```

**`Display` over `Debug` in human-facing messages.** Use `{err}` not `{err:?}` for errors. In `tracing` field syntax, use `%value` not `?value` when the type implements `Display`.

**Log messages must match what the code does.** A message saying "sleeping for 5s" followed by a configurable `poll_interval` that may differ — use the actual value in the message.

**Log task shutdown.** When a `select!` arm exits on `stop_receiver.changed()`, emit `tracing::debug!("ComponentName shutting down")`. Operators need to distinguish clean shutdown from silent disappearance.

**Instrument async spans for significant I/O.** Use `.instrument(tracing::info_span!("operation_name", key = %value))` on futures that perform RocksDB writes, L1 RPC calls, or prover interactions.

### 4.3 Documentation Standards

**User-facing docs** (README, config comments) must not expose internal type names or pipeline topology. Operators care about which env vars to set, what the defaults are, and what happens if a value is wrong — not about `PipelineComponent` impl details.

**Verify every claim in doc comments:**
- Default values in doc comments must match `#[config(default_t = ...)]` in code
- Port numbers in docs must match constants in code
- "Optional" subsystems (batcher, prover API, network) must actually be skipped when their config is absent — verify the conditional in `lib.rs`

**`TODO` / `FIXME` / `// todo: dangling task` comments must be surfaced during review** — either create a tracking issue or fix them in the PR. Do not silently approve code with stub `unimplemented!()` paths that will panic in production.

**Duplicate doc comments are worse than none.** Three sibling impls with identical `///` comments should share a single comment on the trait definition.

### 4.4 Review Process & Communication Style

**Adopt the same severity tier system:**

| Prefix | Meaning |
|---|---|
| `Correctness:` | Bug or data-loss risk — block the PR |
| `Architecture:` | Design concern requiring discussion — block until resolved |
| `Nit:` | Small style issue — suggest a fix, do not block |
| `Bikeshedding:` | Optional preference — flag but do not block |
| `Question:` | Clarification needed — not necessarily requesting a change |
| `FYI:` | Surface for awareness — not a request |

**Provide concrete Rust sketches for architectural suggestions.** Write out the full `impl` block with real types — not pseudocode. This makes the suggestion immediately actionable.

**Defer to domain experts for protocol-level decisions.** If a change touches the VM interface (`zksync_os_interface`), proof verification (`batch_verification`), or L1 contract interactions (`contract_interface`), explicitly request a review from someone with that domain context.

**Acknowledge progress across rounds.** Confirm which previously raised issues are now resolved before raising new ones.

### 4.5 Quick Reference: Red Flags That Always Get Comments

1. `tokio::spawn` inside a library crate without a corresponding `JoinSet` entry in `lib.rs`
2. A `PipelineComponent::run()` that does not exit cleanly when `input.recv()` returns `None`
3. `unwrap_or_default()` where the default is semantically invalid (e.g., `Address::ZERO` for a deployment address)
4. `#[derive(Default)]` on a type where the zero/empty/false state represents failure
5. A background loop with no `stop_receiver` check — will not shut down gracefully
6. Two sequential RocksDB writes that must be atomic but are committed in separate batches
7. `_ => unreachable!()` on enums that may gain new variants (protocol upgrades add new tx types)
8. N+1 RocksDB reads in a per-block loop when a range read would work
9. Changing a config field name or type without a migration note — env var name changes break deployments
10. CPU-bound work (proof generation, Merkle tree recompute) running on the Tokio runtime thread without `spawn_blocking`
11. Log messages that don't match what the code actually executes (wrong value, wrong condition)
12. Tests with no assertions — only `println!` or `dbg!`
13. Bumping `zksync_os_interface` version without back-porting the required changes to all historical `forward_system` versions
14. `// todo: dangling task` — any task that doesn't respect the stop receiver and isn't registered in the `JoinSet`
15. Missing `?` propagation in a fallible RocksDB or RPC call, replaced by `.unwrap()` in a hot path
