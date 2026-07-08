# Live 60-second throughput demo

Hybrid setup: a live browser dashboard as the main screen, plus `btop` in a terminal
beside it showing all cores saturate. The benchmark process itself serves the dashboard
and the Start button — there is nothing to install or copy on the presenter laptop.

## One-time setup (on the benchmark machine)

```bash
apt-get install -y btop
```

Make sure the branch is built and the corpus exists (the first full run generates it,
~15-30 min). Before EVERY demo/rehearsal, pre-warm the corpus into the page cache —
cold corpus reads compete with RocksDB on the benchmark disk:

```bash
cat /root/zksync-os-server/integration-tests/db/corpus*/* > /dev/null
```

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
   ulimit -n 1048576
   export PATH=$HOME/.cargo/bin:$HOME/.foundry/bin:$PATH WORKSPACE_DIR=/root/zksync-os-server
   cd /root/zksync-os-server
   LOAD_TEST_L2_RPC_PORT=8080 LOAD_TEST_DEMO_PORT=7777 \
   ERC20_MAX_TX=2000 LOAD_TEST_READER_THREADS=48 \
   LOAD_TEST_HTTP=1 sequencer_parallel_elide_tree_manager=true \
   RPC_ADMISSION_PROFILE=1 VM_EXECUTE_PROFILE=1 LOAD_TEST_PROMETHEUS_PORT=3312 \
   backpressure_default_block_diff_limit=3072 RPC_CALL_METRICS_SAMPLE=64 \
   LOAD_TEST_WAIT_FOR_RECEIPTS=false LOAD_TEST_FINAL_RECEIPTS=true \
   LOAD_TEST_RPC_LISTENERS=128 general_blocks_to_retain_in_memory=1000000 \
   PARALLEL_BLOCK_LINGER_MS=20 PARALLEL_BLOCKS=192 LOAD_TEST_WALLETS=12288 \
   LOADTEST_TXS_PER_FILE=10000 LOAD_TEST_SUBMIT_PIPELINE=32 \
   RUST_LOG=warn,suite=info,zksync_os_rpc=info \
   LOAD_TEST_DURATION_SECS=60 \
   cargo test --release -p zksync_os_integration_tests --test suite \
   effective_parallel_erc20_tps -- --no-capture
   ```

   It does ~1-2 min of setup (deploy + mints + corpus load), then PARKS and prints
   `DEMO READY — open the dashboard and press Start`.

3. **Open the dashboard**: `http://localhost:7777/` in the presenter's browser. The
   start screen shows "node ready" once it can reach the node.

4. **Side terminal**: `btop` on the machine (second ssh session).

5. **Showtime**: click **Start**. The load begins within a second; the page goes LIVE
   with a T-60s countdown and the rolling transaction odometer; at the end it cuts to a
   full-screen finale card with the totals.

6. **The official number**: when the terminal prints the final line, point at
   `submitted_parallel_tps=...` and `final_receipts_confirmed=12288` — the dashboard is a
   live estimate (±1-2%); the terminal line is the measured, receipt-verified result.

Without `LOAD_TEST_DEMO_PORT` the run starts by itself and the dashboard (opened as a
local file, or via `?rpc=` / `?start=` overrides) just auto-detects the load.

The two profiling envs feed the "transaction journey" panel (per-stage latencies measured
during the run): `RPC_ADMISSION_PROFILE=1` samples 1-in-64 admissions (signature verify /
lane routing, ~free), `VM_EXECUTE_PROFILE=1` accounts VM-busy time once per block. Without
them the panel shows "—" for those stages. Both are server-side: changing them (or any of
this instrumentation) needs a **rebuild** on the machine, unlike dashboard-only tweaks.

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

What it shows (all from the node's exact per-block counters): Transactions / second
(stat + graph with a 1M threshold line), Block height, Blocks / second, Gas / second,
Avg gas / block, Avg txs / block. Prometheus keeps the data after the run ends, so the
natural order is: dashboard finale → terminal logs → flip to Grafana and walk the curves.

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
- **Stale leftovers from a previous failed run**: `pkill -9 -f "deps/suite-"` and
  `rm -rf /tmp/.tmp*` on the machine, then check `df -h /` and `uptime` (load ~0).
- **Always rehearse once** before the meeting: same tunnel, same command, same click.
