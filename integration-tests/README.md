## Integration Tests

This directory contains a fairly simple framework designed to test ZKsync OS node's behavior e2e on a local setup.

Main design considerations:
* **Isolation**. Every test initializes its own node, L1, persistence resources etc. Meaning you can run every test independently of each other.
* **Speed**. Every test environment spins up very quickly (<3 seconds ) thanks to pre-initialized L1 with some reasonable defaults.
* **ZKsync-agnostic**. Test try to use general Ethereum tooling where this makes sense (e.g., base `alloy`) to enforce our compatibility with Ethereum.
* **Rust-first**. No reliance on external tooling to run the tests, regular Rust flows should work as expected. `RUST_LOG`/`RUST_BACKTRACE` get propagated to all relevant components (node, prover).

Known limitations:
* No support for node restarts - not a fundamental issue, can be added in the future
* No L1 logs - `alloy` provider swallows `anvil` logs by default with no option to disable this behavior, but this can be solved by writing our own `anvil` spawner
* No graceful shutdown - support is blocked by the node itself not doing graceful shutdown yet

### Run

Pre-requisites:
* Make sure you have `cargo-nextest` version `0.9.101` or later installed in your system as we use some recently added features.
* Make sure you have `forge` installed to be able to build test contracts (see [README](./test-contracts/README.md)).
* Use `RUST_BACKTRACE=1` (or `RUST_BACKTRACE=full`) to enable backtraces.
* Configure logging level via `RUST_LOG=debug` (or `RUST_LOG=info,zksync_os_sequencer=debug` for specific components).

#### Examples

```shell
# All tests (excluding prover)
cargo nextest run -p zksync_os_integration_tests

# Single test with exact name match
cargo nextest run -p zksync_os_integration_tests -E 'test(=basic_transfers)'
# If you want logs to be printed during the test:
cargo nextest run -p zksync_os_integration_tests -E 'test(=basic_transfers)' --no-capture
# If you want all tests containing a substring:
cargo nextest run -p zksync_os_integration_tests -E 'test(~call)'

# All tests inside `./tests/call.rs`
cargo nextest run -p zksync_os_integration_tests -E 'binary(call)'

# Run prover tests (CPU prover)
# !! Note `--release`, important to avoid stack overflow and low performance in prover !!
cargo nextest run --release -p zksync_os_integration_tests --features prover-tests -E 'binary(prover)'

# Run prover tests (GPU prover)
# !! Note `--release`, important to avoid stack overflow and low performance in prover !!
# !! Requires 24GB of VRAM and CUDA toolkit 12.x installed !!
cargo nextest run --release -p zksync_os_integration_tests --features gpu-prover-tests -E 'binary(prover)'
```

### DvP load scenario

The DvP baseline exchanges 3 units of an asset ERC-20 for 7 units of a cash ERC-20
in one atomic transaction. Each buyer/seller pair has a fixed-terms settlement
contract and approves a finite trading budget. Both tokens use `TestERC20`;
amounts are raw token units. Only the buyer can settle, and a failed payment
rolls back delivery as well as allowance changes and logs.

```shell
# Correctness: repeated settlement, failure of either leg, and unauthorized calls.
cargo nextest run -p zksync_os_integration_tests --profile no-pig \
  -E 'test(~load::dvp::dvp_)'

# Short multi-pair run, including exact final balance / allowance checks for every pair.
RUST_LOG=info LOAD_TEST_DURATION_SECS=5 LOAD_TEST_DVP_PAIRS=4 \
  LOAD_TEST_DVP_MAX_SWAPS_PER_PAIR=20 LOAD_TEST_CONCURRENCY=4 \
  cargo nextest run -p zksync_os_integration_tests --profile no-pig \
  -E 'test(~effective_dvp_tps)' --no-capture
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOAD_TEST_DURATION_SECS` | 10 | Maximum submission window, excluding setup |
| `LOAD_TEST_DVP_PAIRS` | 4 | Independent buyer/seller pairs |
| `LOAD_TEST_DVP_MAX_SWAPS_PER_PAIR` | 100 | Settlement cap and funded/approved budget per pair |
| `LOAD_TEST_CONCURRENCY` | 4 | Global maximum of in-flight settlements |

Each pair has at most one settlement in flight. Submission ends at the duration
or the per-pair cap, whichever comes first. Transactions are signed on demand and
sent through RPC and the normal mempool with serial block execution, zero fees,
and replay records retained for state verification. No transaction corpus is needed.
The scenario also enables `general_retain_receipt_logs` for its node: this benchmark
branch otherwise drops receipt logs. Existing throughput scenarios keep their lean
receipts; this option retains logs only in the live receipt window.

The result reports successful swaps per second over submission plus receipt drain,
average receipt latency, average gas per swap, and the time from the last accepted
submission until all receipts have arrived. Deployment, minting, approvals, and
final state queries are excluded from timing. One swap counts as one transaction,
not two transfers. Receipt/event failures, timeouts, state mismatches, or zero
completed swaps fail the run; there is no TPS threshold. This is a functional
baseline, not a measurement of maximum sequencer throughput.

### Parallel DvP throughput

`effective_parallel_dvp_tps` runs the same contract through the existing
`effective_parallel_impl` harness used by the native/ERC-20 throughput tests:
cached real-signed transactions, HTTP batches, independent chunk pipelines,
multiple RPC listeners, and parallel sequencer lanes. Each buyer has an exclusive
seller and settlement contract; the two token contracts are shared, but all
balance and allowance writes are disjoint between buyers. Setup approvals are
accounted for in the corpus's starting nonces and fingerprint.

For a local throughput run:

```shell
RUST_LOG=warn,suite=info,suite::load::corpus=error TOKIO_WORKER_THREADS=32 \
  LOADTEST_CORPUS_DIR="$PWD/db/dvp-corpus" LOADTEST_TXS_PER_FILE=30000 \
  LOAD_TEST_DURATION_SECS=60 LOAD_TEST_WALLETS=512 \
  LOAD_TEST_HTTP=1 LOAD_TEST_RPC_LISTENERS=16 LOAD_TEST_SUBMIT_PIPELINE=32 \
  LOAD_TEST_READER_THREADS=8 LOAD_TEST_WAIT_FOR_RECEIPTS=false LOAD_TEST_FINAL_RECEIPTS=true \
  PARALLEL_BLOCKS=16 PARALLEL_BLOCK_LINGER_MS=20 DVP_MAX_TX=500 \
  general_state_backend=InMemory general_blocks_to_retain_in_memory=4096 \
  sequencer_parallel_elide_tree_manager=true backpressure_default_block_diff_limit=3072 \
  RPC_CALL_METRICS_SAMPLE=64 \
  cargo nextest run --release -p zksync_os_integration_tests --test suite --profile no-pig \
  -E 'test(~effective_parallel_dvp_tps)' --no-capture
```

This is the benchmark configuration: in-memory execution state, no tree processing
or proving, and lean receipts. The replay WAL remains enabled for final state reads.
Each wallet's last receipt is awaited, then **every pair's exact balances,
allowances, nonce, and the total token supplies are checked against the accepted
transaction counts**, including partially accepted final batches. The reported
`verified_dvp_tps` divides that verified swap count by submission plus receipt-drain
time; corpus generation, setup, and final state queries are excluded. A final
receipt alone would not prove that earlier swaps succeeded, so state verification
is mandatory even with per-transaction receipt polling disabled.

`DVP_MAX_TX` defaults to 250 (the command uses 500). Keep this below the VM's native
cycle sealing limit: overflowing a direct lane can drop transactions and leave
nonce gaps. Keep `LOADTEST_TXS_PER_FILE` large enough to sustain the whole run;
corpus exhaustion ends submission early. The serial correctness tests above retain
full event/rollback coverage independently of this throughput configuration.

Local reference (2026-09-21, Ryzen 9 9950X3D, release build): the command above
settled and verified 11,909,984 swaps in 61.008 seconds including receipt drain,
or **195,221 swaps/s**. A preceding 60-second run with a larger receipt cache
reached 192,270 swaps/s. These numbers include RPC admission and execution in the
benchmark configuration described above, not tree processing or L1 settlement.

On the Threadripper PRO 9985WX (64 cores / 128 threads, Vast instance 51893085),
use the following configuration from the repository root. Leave CPU affinity
unrestricted and unset `TOKIO_WORKER_THREADS`. The measured run used tmpfs for
the corpus and temporary databases; it includes RocksDB execution-state access,
but does not measure physical-disk durability or tree/prover throughput.

```shell
mkdir -p /dev/shm/dvp-corpus /dev/shm/dvp-bench-tmp
ulimit -n 65536
env -u TOKIO_WORKER_THREADS -u LOAD_TEST_FLAMEGRAPH \
  TMPDIR=/dev/shm/dvp-bench-tmp LOADTEST_CORPUS_DIR=/dev/shm/dvp-corpus \
  LOADTEST_TXS_PER_FILE=60000 LOAD_TEST_DURATION_SECS=60 LOAD_TEST_WALLETS=1024 \
  LOAD_TEST_HTTP=1 LOAD_TEST_RPC_LISTENERS=128 LOAD_TEST_SUBMIT_PIPELINE=32 \
  LOAD_TEST_READER_THREADS=48 LOAD_TEST_WAIT_FOR_RECEIPTS=false LOAD_TEST_FINAL_RECEIPTS=true \
  PARALLEL_BLOCKS=192 PARALLEL_BLOCK_LINGER_MS=20 DVP_MAX_TX=500 \
  sequencer_block_time=250ms general_state_backend=FullDiffs \
  general_blocks_to_retain_in_memory=1000000 \
  sequencer_parallel_elide_tree_manager=true sequencer_parallel_skip_replay_wal=false \
  backpressure_default_block_diff_limit=3072 RPC_CALL_METRICS_SAMPLE=64 \
  RUST_LOG=warn,suite=info,suite::load::corpus=error \
  cargo test --locked --release -p zksync_os_integration_tests --test suite \
  effective_parallel_dvp_tps -- --nocapture
```

Reference result (2026-09-21): **391,116 verified swaps/s**, with 23,580,960 swaps
in 60.292 seconds including receipt drain and no exhausted wallets. Each swap executes both ERC-20 legs;
the TPS count is swaps, not individual token transfers. All 1,024 final receipts
and every pair's state checks passed. Two earlier 60-second runs with 30,000
transactions per wallet reached 407,784 and 408,079 swaps/s, but the repeat run
exhausted 256 wallets; use the larger corpus to keep every wallet supplied.

The parallel DvP setup batches deployments, mints, and approvals, and checks every
setup receipt. It preserves the original deployment nonce order, so previously
generated corpora remain valid. The old setup forced `block_time` to 5 ms, which
also capped parallel block age despite `PARALLEL_BLOCK_LINGER_MS=20`. Removing
that override increased average block occupancy from 141 to 378 swaps in matched
1,024-wallet profiles and raised throughput from 291k to 387k swaps/s. Restoring
only the 5 ms cap in the batched harness reproduced 291k swaps/s and 142 swaps per
block. Keep block age separate from the idle linger when tuning.

Wallet count also matters: the ERC-20 command's 12,288 wallets are not a good
starting point for this DvP workload. With normal block timing, that configuration
verified 127k swaps/s in a 20-second run; 1,024 wallets reached the result above.
Use `LOAD_TEST_FLAMEGRAPH=/path/to/profile.svg` and `PARALLEL_PRODUCER_PROFILE=1`
with `zksync_os_sequencer::execution::block_executor=info` in `RUST_LOG` to inspect
CPU costs and block occupancy. Profiles stop at receipt drain, before final state
queries. The `per-wallet corpus usage` summary reports exhausted wallets; increase
`LOADTEST_TXS_PER_FILE` if any wallet runs out during a longer/faster run.
