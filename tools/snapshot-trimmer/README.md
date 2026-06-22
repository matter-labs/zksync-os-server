# snapshot-trimmer

Reduces an 8 GB+ zksync-os-server node DB snapshot to a small bootstrap artifact
(~1 GB) suitable for storing in S3 and auto-downloading in a staging/dev setup.

## Build

```bash
cargo build --release -p zksync_os_snapshot_trimmer
# binary: target/release/snapshot-trimmer
```

## Usage

```bash
./target/release/snapshot-trimmer --db-dir <path>
```

`--db-dir` accepts either:
- the node DB directory itself (contains `block_replay_wal/`, `repository/`, …), or
- a snapshot **parent** directory containing a `node1/` subdir (e.g. a freshly
  `kubectl cp`'d folder that also holds `fri_proofs/`, `block_dumps/`, …).

By default the tool:
1. **Cleans up** non-node siblings (`fri_proofs/`, `block_dumps/`, …) when given a parent dir.
2. **Normalizes** the sub-DBs to a common consistent block height — fixes snapshots copied
   from a *running* node, where the sub-DBs (WAL/state/tree/repository) sit at slightly
   different heights and would otherwise panic the server with a "historical write discrepancy".
3. **Trims** `block_replay_wal`, `repository`, and `state_full_diffs` to the last
   `--keep-blocks` blocks (default 2000).
4. **GCs** the merkle tree (keeps only nodes reachable from the kept block range).
5. **Compacts** every touched column family to reclaim disk space immediately.

Preimages and batch DBs are left untouched. Preview any run with `--dry-run`.

## Flags

- `--keep-blocks N` (default `2000`) — recent blocks to keep. Must exceed the gap between the
  WAL tip and `last_l1_executed_block` at snapshot time (the L1 execution lag), or the server
  panics on startup with `Unless it's a new chain, replay record must exist`. That lag isn't
  stored in the snapshot, so values below 1000 are rejected unless you pass `--allow-small-keep`.
- `--no-cleanup` / `--no-normalize` / `--no-trim-tree` / `--no-compact` — opt out of a default step.
- `--dry-run` — report what would change without writing.
- `--print-batch-for-block [N]` — **read-only**, no trimming. Prints which batch covers block `N`
  (defaults to the snapshot tip) and how to pick the matching L1 fork block. No L1 connection
  required; the L1 hop is emitted as a `cast` recipe.

## Notes

- **Operates in place.** Always run on a *copy* — never on an original/golden snapshot.
- A trimmed snapshot boots a main node only if **L1's committed tip ≤ the snapshot tip**. If you
  run against a forked L1 (e.g. anvil), use `--print-batch-for-block` to find the L1 block to fork
  at so L1 isn't ahead of the snapshot.
