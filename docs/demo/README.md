# Live 60-second atomic DvP demo

The live dashboard shows delivery versus payment (DvP): a seller delivers **3 ASSET**
to a buyer, and the buyer pays **7 CASH** to the seller in the same transaction.
Both legs succeed or both revert. One swap is one transaction containing two
ERC-20 transfers; count swaps once, not once per leg. Each pair has its own
settlement contract, balances, and finite allowances.

The benchmark serves the dashboard and its Start button. Keep `btop` in a second
terminal to show CPU activity. The chart's **400k swaps/s** line is a target, not a
guarantee: the reference 9985WX run verified **391,116 swaps/s** over a full minute,
including receipt drain, with no exhausted wallets. See the
[benchmark configuration and results](../../integration-tests/README.md#parallel-dvp-throughput).
The demo's optional profiling can affect throughput.

## One-time setup (on the benchmark machine)

Build the branch with the usual Rust / Foundry prerequisites and install `btop`:

```bash
apt-get install -y btop
cargo test --locked --release -p zksync_os_integration_tests --test suite --no-run
```

Run from the repository root. The command below stores its DvP corpus and temporary
node databases in tmpfs, matching the reference run. Allow enough RAM for the
corpus, node state, and retained receipts. The first run generates the signed corpus;
later runs reuse it. Rehearse once before presenting and wait for `DEMO READY`.
There is no need to pre-warm this tmpfs corpus from disk.

This is an RPC + sequencer execution benchmark: FullDiffs/RocksDB state and replay
WAL remain enabled; tree processing, proving, and L1 settlement are disabled.
With databases in tmpfs it does not measure physical-disk durability.

## Demo-day sequence

1. **Tunnel** (on the presenter laptop) — 8080 carries the node RPC, 7777 the dashboard.
   Give it its OWN terminal, with no shell:

   ```bash
   ssh -q -N -p <PORT> root@<HOST> -L 8080:localhost:8080 -L 7777:localhost:7777 -L 3000:localhost:3000
   ```

   (3000 carries Grafana — drop it if you skip the Grafana finale.)

   `-q` silences the `channel N: open failed: connect failed` lines the local ssh client
   prints whenever the dashboard polls through the forwards while the node is down
   (before setup finishes and after teardown); `-N` makes it a pure tunnel. Run the test
   in a SEPARATE plain ssh session (no `-L`) — combining tunnel and run shell in one
   session without `-q` splats that noise into the demo terminal at the end of the run.

2. **Run** (on the machine, started EARLY — before the meeting moment). The two demo
   envs are `LOAD_TEST_L2_RPC_PORT=8080` (pins the RPC port to the tunnel) and
   `LOAD_TEST_DEMO_PORT=7777` (serves the dashboard + parks the run on the Start button):

   ```bash
   ulimit -n 65536
   export PATH="$HOME/.cargo/bin:$HOME/.foundry/bin:$PATH"
   export WORKSPACE_DIR="$PWD"
   mkdir -p /dev/shm/dvp-corpus /dev/shm/dvp-demo-tmp
   env -u TOKIO_WORKER_THREADS -u LOAD_TEST_FLAMEGRAPH \
   LOAD_TEST_L2_RPC_PORT=8080 LOAD_TEST_DEMO_PORT=7777 \
   TMPDIR=/dev/shm/dvp-demo-tmp LOADTEST_CORPUS_DIR=/dev/shm/dvp-corpus \
   DVP_MAX_TX=500 LOAD_TEST_READER_THREADS=48 \
   LOAD_TEST_HTTP=1 sequencer_parallel_elide_tree_manager=true \
   sequencer_block_time=250ms general_state_backend=FullDiffs \
   sequencer_parallel_skip_replay_wal=false \
   RPC_ADMISSION_PROFILE=1 VM_EXECUTE_PROFILE=1 LOAD_TEST_PROMETHEUS_PORT=3312 \
   backpressure_default_block_diff_limit=3072 RPC_CALL_METRICS_SAMPLE=64 \
   LOAD_TEST_WAIT_FOR_RECEIPTS=false LOAD_TEST_FINAL_RECEIPTS=true \
   LOAD_TEST_RPC_LISTENERS=128 general_blocks_to_retain_in_memory=1000000 \
   PARALLEL_BLOCK_LINGER_MS=20 PARALLEL_BLOCKS=192 LOAD_TEST_WALLETS=1024 \
   LOADTEST_TXS_PER_FILE=60000 LOAD_TEST_SUBMIT_PIPELINE=32 \
   RUST_LOG=warn,suite=info,suite::load::corpus=error \
   LOAD_TEST_DURATION_SECS=60 \
   cargo test --locked --release -p zksync_os_integration_tests --test suite \
   effective_parallel_dvp_tps -- --nocapture
   ```

   It deploys the tokens and settlement contracts, mints budgets, approves both legs,
   and prepares the corpus, then PARKS and prints
   `DEMO READY — open the dashboard and press Start`.

3. **Open the dashboard**: `http://localhost:7777/` in the presenter's browser. The
   start screen shows "node ready" once it can reach the node.

4. **Side terminal**: `btop` on the machine (second ssh session).

5. **Showtime**: click **Start DvP**. The load begins within a second; the page goes LIVE
   with a T-60s countdown, live **TPS**, and a rolling **Atomic swaps** odometer. The graph stops at
   the server's submission deadline, then the page shows **draining receipts** while
   collecting the final counters. Idle samples after the deadline are outside the plotted
   load window; throughput dips within it remain visible. The final board freezes the
   submission estimates; it does not claim that state verification has finished.

6. **The official number**: when the terminal prints the final line, point at
   **`verified_dvp_tps=...`** in the `parallel DvP balances, allowances, nonces and
   supplies verified` line, followed by a passing test result. The harness checks every
   pair's exact balances, allowances and buyer nonce, plus both total supplies. Confirm
   `final_receipts_confirmed=1024` and `exhausted_wallets=0`. The verified rate includes
   receipt drain; the browser's submission-window estimate is not that final rate.

Without `LOAD_TEST_DEMO_PORT` the run starts by itself and the dashboard (opened as a
local file, or via `?rpc=` / `?start=` overrides) just auto-detects the load.

The two profiling envs feed the "atomic swap journey" panel (per-stage latencies measured
during the run): `RPC_ADMISSION_PROFILE=1` samples 1-in-64 admissions (signature verify /
lane routing, ~free), `VM_EXECUTE_PROFILE=1` accounts VM-busy time once per block. Without
them the panel shows "—" for those stages. Set these flags before starting the test;
changing their values requires a restart, not a rebuild. Editing Rust instrumentation requires a rebuild. Dashboard-only edits
are loaded from disk when the test starts. The journey shows stage timings and an
approximate stage sum, not measured end-to-end settlement latency.

## Grafana finale (optional)

Show real node metrics after the run: the test exposes the in-process node's Prometheus
metrics on `LOAD_TEST_PROMETHEUS_PORT` (3312 in the canonical command), a local Prometheus
scrapes them every second, and Grafana serves the repo's sequencer dashboard.

One-time setup on the machine (downloads static tarballs — no docker needed):

```bash
bash docs/demo/grafana/setup.sh
```

Before each demo (idempotent, kills previous instances):

```bash
bash docs/demo/grafana/start.sh
```

Then open `http://localhost:3000/d/zksync-demo` through the tunnel (anonymous admin,
no login). The dashboard defaults to **Last 5 minutes / 5s refresh**.

What it shows (all from the node's exact per-block counters): Executed swaps / second
(stat + graph with a 400k target line), Block height, Blocks / second, Gas / second,
Avg gas / block, Avg swaps / block. During setup these counters also include
deployments, mints and approvals; interpret swap rates over the timed load window.
Grafana execution counters do not replace the terminal state checks. Prometheus
keeps the data after the run ends, so the natural order is: dashboard finale → terminal logs → flip to Grafana and walk the curves.

Note: the repo-root `grafana_dashboard.json` (full sequencer dashboard) predates the
current metric names — its queries come back empty against this node, which is why the
demo provisions its own `demo-dashboard.json`.

### Importing a production dashboard export

A production Grafana export (the v2 `apiVersion/kind/metadata/spec` JSON) won't work
locally as-is: its queries point at the production datasource via `${cluster}` and filter
on k8s labels (`namespace`, `pod`, …) the local scrape doesn't have. Localize it first:

```bash
python3 docs/demo/grafana/localize-dashboard.py <prod-export.json> \
  docs/demo/grafana/prod-dashboard-local.json
```

(pins the datasource variables to the local Prometheus, converts the k8s query variables
to hidden `.*` constants — regex matchers match label-less series — and resets the uid to
`zksync-prod-local`). Then either:

- **Provisioning (preferred)**: copy the output to `~/demo-observability/dashboards/` on
  the machine and re-run `start.sh` — Grafana picks it up at boot. It's then at
  `http://localhost:3000/d/zksync-prod-local`. Fresh `setup.sh` runs copy it automatically.
- **UI import**: Grafana → Dashboards → New → Import → upload the localized JSON.

Expect panels for disabled components (prover, batcher, L1 senders, kube-state) and any
recording-rule queries (`sequencer:…`) to stay empty — the API / execution / state
sections are the live ones. The API row under-reads ~64× (`RPC_CALL_METRICS_SAMPLE=64`).

## Troubleshooting

- **Port busy on the machine**: pick others (`LOAD_TEST_L2_RPC_PORT=9090
  LOAD_TEST_DEMO_PORT=7778`) and re-tunnel accordingly.
- **Dashboard stuck on *connecting to node***: the benchmark isn't running yet, or the
  tunnel dropped — reconnect ssh; the page retries automatically.
- **Stale leftovers from a previous failed run**: identify and stop that test process,
  then inspect its temporary directory under `/dev/shm/dvp-demo-tmp`. Remove only
  that stopped run’s files; preserve the reusable `/dev/shm/dvp-corpus`.
- **Input runs out early**: check `per-wallet corpus usage`; increase
  `LOADTEST_TXS_PER_FILE` if `exhausted_wallets` is nonzero. Keep the normal 250 ms
  block age: a 5 ms setup override also caps parallel blocks and hurts DvP throughput.
- **Always rehearse once** before the meeting: same tunnel, same command, same click.
