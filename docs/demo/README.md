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

1. **Tunnel** (on the presenter laptop) — 8080 carries the node RPC, 7777 the dashboard:

   ```bash
   ssh -q -p <PORT> root@<HOST> -L 8080:localhost:8080 -L 7777:localhost:7777
   ```

   (`-q` silences the harmless `channel open failed` lines from polling before the node
   is up.)

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
   RPC_ADMISSION_PROFILE=1 VM_EXECUTE_PROFILE=1 \
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

## Troubleshooting

- **Port busy on the machine**: pick others (`LOAD_TEST_L2_RPC_PORT=9090
  LOAD_TEST_DEMO_PORT=7778`) and re-tunnel accordingly.
- **Dashboard stuck on *connecting to node***: the benchmark isn't running yet, or the
  tunnel dropped — reconnect ssh; the page retries automatically.
- **Stale leftovers from a previous failed run**: `pkill -9 -f "deps/suite-"` and
  `rm -rf /tmp/.tmp*` on the machine, then check `df -h /` and `uptime` (load ~0).
- **Always rehearse once** before the meeting: same tunnel, same command, same click.
