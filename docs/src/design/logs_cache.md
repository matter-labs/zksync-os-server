# Recent Logs Cache MVP

## Goal

Introduce a shared in-memory `LogsCache` for recent settlement-layer blocks so all L1 watchers can
reuse the same fetched logs instead of issuing overlapping `eth_getLogs` RPCs.

The MVP is intentionally narrow:

* `LogsCache` becomes the sole source of logs for watchers.
* It only accelerates recent-block queries.
* It falls back to the provider whenever the requested range is not fully covered.
* It keeps full raw logs per cached block and filters on read.

This is not a general indexing layer, not a historical scanner, and not a replacement for
provider-backed fetching outside the retained recent window.

## Non-goals

The MVP does **not** attempt to solve:

* historical scanning with alternative backends
* pre-filtered or per-watcher indexes
* websocket / push ingestion
* cross-process or cross-node cache sharing
* refactoring all watchers into one orchestrator
* abstracting log access behind an additional trait

## Why this shape

The current watcher system already has the right processing abstraction: each watcher owns its own
cursor, boundary semantics, and event processor. The expensive duplication is in log retrieval, not
in event handling.

The MVP therefore changes only one thing:

* watchers stop calling the provider directly for logs
* watchers call `LogsCache::get_logs(&Filter)` instead

This keeps watcher-specific behavior intact while making recent-block log fetching shared and
reorg-aware.

## API

`LogsCache` should expose the same API shape currently used by the watchers:

```rust
impl LogsCache {
    pub async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>>;
}
```

This keeps watcher wiring simple: `L1Watcher` can receive `LogsCache` directly and use it as the
sole log source.

## High-level behavior

`LogsCache` owns:

* a provider
* a `watch::Receiver<BlockUpdates>`
* a bounded recent-block window
* the last `BlockUpdates` snapshot the cache has synchronized to

Before serving each query, `LogsCache` first synchronizes its recent-block window to the freshest
known `BlockUpdates` state if and only if the head snapshot has changed since the last
synchronization. It then:

* serves the query from memory if the full requested block range is covered by the cache
* otherwise falls back to provider `eth_getLogs`

The cache is always best-effort and opportunistic. It must never change watcher behavior for
out-of-window queries.

## Data model

The cache stores a contiguous suffix of recent canonical blocks.

```rust
struct CachedBlockLogs {
    hash: B256,
    logs: Vec<Log>,
}

struct RecentBlocks {
    synced_with: BlockUpdates,
    first_block: Option<u64>,
    blocks: VecDeque<CachedBlockLogs>,
}

struct LogsCache {
    provider: DynProvider,
    block_updates: watch::Receiver<BlockUpdates>,
    recent: RwLock<RecentBlocks>,
    capacity: usize,
}
```

Block numbers are **not** stored inside each deque entry. Instead:

* `first_block` gives the number of `blocks[0]` when the deque is non-empty
* block number of `blocks[i]` is `first_block + i as u64`

This keeps the model compact while still allowing O(1) mapping between block number and deque
offset.

### Cache invariants

At all times:

* `blocks` represents a contiguous block-number range
* `blocks` corresponds to one canonical chain suffix
* `blocks.len() <= capacity`
* `synced_with` is the most recent `BlockUpdates` snapshot the cache has already processed
* `first_block` is `Some(_)` if and only if `blocks` is non-empty

## Capacity

The initial default should be `128` blocks, configurable through `L1WatcherConfig` or adjacent
configuration.

This is intentionally generous for the MVP:

* it covers the current mix of finalized and confirmation-delayed watchers
* it gives room for naive reorg handling and slight watcher lag
* it avoids premature optimization around pre-filtering or partial-range stitching

Storing full logs for 128 recent blocks may be somewhat heavy, but is acceptable for the MVP.

## Synchronization model

`LogsCache` updates itself lazily on demand.

Whenever `get_logs()` is called:

1. Read the latest available `BlockUpdates` snapshot from the watch receiver.
2. If the cache is already synchronized to that exact snapshot, skip synchronization entirely.
3. Otherwise bring the internal cache state up to date with that head.
4. Serve or fall back.

With `tokio::sync::watch`, this means serving against the latest observed value from the receiver,
not replaying intermediate updates one by one and not blocking waiting for a future update.

This means queries are served from the freshest known head, not from a separately ticking background
task.

## Which head is cached

The cache should track recent chain head blocks, not only finalized blocks.

That means:

* recent confirmed-but-not-finalized watchers can reuse cached data
* finalized watchers can query the same cache if their requested blocks are still retained
* there is no separate finalized-only cache in the MVP

This is consistent with current watcher behavior, where different watchers already choose their own
block boundary semantics on top of the same settlement-layer chain.

## Fetch strategy

The cache is maintained block-by-block.

For each block that needs to enter the recent window:

1. fetch the block header by number
2. fetch that block's logs
3. append `{hash, logs}` to the deque

The header fetch provides the block hash needed for canonicality checks and recent reorg handling.

For simplicity, the MVP may fetch logs using either:

* a single-block `from_block == to_block == N` filter, or
* a `blockHash`-pinned filter

Using `blockHash` is preferable when convenient because it ties the fetched logs to the exact
canonical block chosen during synchronization.

The extra header request is an acceptable tradeoff in the MVP because correctness and simple reorg
handling matter more than minimizing per-block RPC count at this stage.

## Reorg handling

The cache performs naive recent reorg handling.

When synchronizing against a newer head:

1. Compare the cached tip against the current chain view.
2. If the cached tip hash no longer matches the chain at that block number, pop cached blocks from
   the back until a common ancestor is found.
3. Log a warning with the detected reorg depth.
4. Refill the deque forward until it reaches the current head.

This policy is intentionally conservative:

* warn, do not panic
* rewind only the recent cached suffix
* continue serving queries after refill

### When to escalate

The MVP should only escalate if:

* no common ancestor is found within the retained window
* a supposedly finalized block appears to change
* provider responses are internally inconsistent

Normal shallow recent reorgs should not be treated as fatal.

## Query eligibility

A query can be served from cache only if the full requested block selection is covered.

For the MVP, cache-serving should be intentionally narrow and should only handle the watcher hot
path. A query is cache-eligible only if it is a bounded block-range filter in the same shape the
current watchers already produce.

Specifically, cache-serving should require:

* finite `from_block` and `to_block`
* no `blockHash`
* every requested block to be present in the cache window
* a filter shape that the in-memory matcher supports

If any of these checks fail, `LogsCache` should fall back to provider `eth_getLogs` for the whole
request, even if most of the range is cached.

This avoids complicated partial stitching in the MVP.

## In-memory filtering

The cache stores full raw logs and applies the filter in memory when serving covered queries.

The MVP should support the filter shapes currently used by watchers:

* `from_block`
* `to_block`
* address or address-array filtering
* topic0 event signatures
* optional `topic1`

The filtering logic should be intentionally minimal and only match the subset of `Filter`
semantics that the watchers already rely on. If a caller passes an unexpected filter shape
(`blockHash`, unsupported topic combinations, open-ended ranges, etc.), `LogsCache` should forward
the whole request to the provider instead of trying to handle corner cases.

The implementation should lean on Alloy's existing `Filter`, `Log`, and topic/address types as much
as practical for parsing and comparing fields. However, cache eligibility, block-range coverage, and
"does this request belong to the supported watcher subset?" remain `LogsCache` responsibilities.
The MVP should not assume Alloy provides a complete in-memory `Filter` x `Vec<Log>` matcher for the
exact subset we need.

## Interaction with `BlockUpdates`

`BlockUpdates` remains the shared source of head information. `LogsCache` consumes it directly.

This separation is useful:

* `BlockUpdates` tells consumers what the latest and finalized block numbers are
* `LogsCache` materializes a recent canonical suffix of logs derived from that head information

## Interaction with watchers

No watcher-specific pre-registration is needed in the MVP.

Each watcher continues to own:

* its cursor (`next_block`)
* its boundary semantics (`confirmed` vs `finalized`)
* its processor and event decoding

The only change is that `L1Watcher` uses `LogsCache` instead of the raw provider when calling
`get_logs`.

This keeps the integration small and localized while still eliminating overlapping recent-block
fetches.

## Concurrency

`LogsCache` should own its recent-block state behind an async `RwLock`.

This is a better fit than a plain mutex for the expected access pattern:

* all watchers wake up from the same `BlockUpdates` change
* the first query after a new head will perform the write-side synchronization
* subsequent concurrent queries are likely to be read-only against the same cached snapshot

The synchronization path should therefore use a double-check pattern:

1. read-lock and compare `synced_with` to the latest observed `BlockUpdates`
2. if already synchronized, stay read-only
3. otherwise drop the read lock, take the write lock, re-check, and synchronize only once

The main downside versus a mutex is slightly more bookkeeping around the read-to-write transition.
That extra complexity is acceptable because it avoids making every concurrent post-update query wait
behind a read-only fast path.

## Fallback behavior

Fallback is a core part of the design, not an exceptional path.

The provider remains the source of truth for:

* uncached historical ranges
* filter shapes the cache does not support
* any request the cache cannot answer with confidence

This guarantees that the cache is a safe optimization layer rather than a behavior-changing
subsystem.

## Expected simplification

This design reduces complexity in a relevant place without requiring a watcher rewrite.

It should simplify `L1Watcher` by letting it stop caring about:

* whether a recent block was already fetched by another watcher
* how recent shallow reorgs are absorbed during log retrieval

Watchers stay responsible for sequencing and processing. `LogsCache` becomes responsible for recent
shared log retrieval and canonical recent-block materialization.

## Possible follow-ups after MVP

Once the MVP is stable, the most natural next extensions are:

* pre-filtered secondary indexes if raw-log scans become too expensive
* a separate historical-scanner backend for far-behind watchers
* receipts-based recent-block ingestion if it benchmarks better than `eth_getLogs`
* extracting the cache / provider decision into a more explicit backend-selection layer

Those should be evaluated after the recent-block cache proves its value in production.
