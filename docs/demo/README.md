# Live 60-second throughput demo

Hybrid setup: a live browser dashboard (`live-dashboard.html`) as the main screen, plus
`btop` in a terminal beside it showing all cores saturate.

## One-time setup (on the benchmark machine)

```bash
apt-get install -y btop
```

Make sure the branch is built and the corpus exists (the first full run generates it,
~15–30 min). Before EVERY demo/rehearsal, pre-warm the corpus into the page cache —
cold corpus reads compete with RocksDB on the benchmark disk:

```bash
cat /root/zksync-os-server/integration-tests/db/corpus*/* > /dev/null
```

## Demo-day sequence

1. **Tunnel** (on the presenter laptop) — port 8080 for the node RPC, 8081 for the
   Start button's signal helper:

   ```bash
   ssh -q -p <PORT> root@<HOST> -L 8080:localhost:8080 -L 8081:localhost:8081
   ```

   (`-q` silences the harmless `channel open failed: connection refused` lines the
   dashboard's polling produces before the node is up.)

1b. **Start-signal helper** (on the machine, once per session):

   ```bash
   python3 /root/zksync-os-server/docs/demo/start-server.py &
   ```

2. **Open the dashboard**: open `docs/demo/live-dashboard.html` in a browser (double-click
   the file). It shows *waiting for load* until blocks start flowing. A different RPC
   endpoint can be passed as `live-dashboard.html?rpc=http://127.0.0.1:9090`.

3. **Side terminal**: `btop` on the machine (via a second ssh session).

4. **Run** (on the machine) — the canonical 60s benchmark with the RPC port pinned to the
   tunnel (`LOAD_TEST_L2_RPC_PORT=8080` is what makes the dashboard work):

   ```bash
   ulimit -n 1048576
   export PATH=$HOME/.cargo/bin:$HOME/.foundry/bin:$PATH WORKSPACE_DIR=/root/zksync-os-server
   cd /root/zksync-os-server
   LOAD_TEST_L2_RPC_PORT=8080 LOAD_TEST_START_GATE=/tmp/demo_start \
   ERC20_MAX_TX=2000 LOAD_TEST_READER_THREADS=48 \
   LOAD_TEST_HTTP=1 sequencer_parallel_elide_tree_manager=true \
   backpressure_default_block_diff_limit=3072 RPC_CALL_METRICS_SAMPLE=64 \
   LOAD_TEST_WAIT_FOR_RECEIPTS=false LOAD_TEST_FINAL_RECEIPTS=true \
   LOAD_TEST_RPC_LISTENERS=128 general_blocks_to_retain_in_memory=1000000 \
   PARALLEL_BLOCK_LINGER_MS=20 PARALLEL_BLOCKS=192 LOAD_TEST_WALLETS=12288 \
   LOADTEST_TXS_PER_FILE=10000 LOAD_TEST_SUBMIT_PIPELINE=32 RUST_LOG=warn \
   LOAD_TEST_DURATION_SECS=60 \
   cargo test --release -p zksync_os_integration_tests --test suite \
   effective_parallel_erc20_tps -- --no-capture
   ```

   Timeline: start the command EARLY (before the meeting moment) — it does ~1–2 min of
   setup (deploy + mints + corpus load) and then PARKS, printing `DEMO READY — waiting
   for start signal`. The dashboard's start screen shows "node ready". At showtime,
   click **Start** on the dashboard: the load begins within a second, the page flips to
   **LIVE** with a T−60s countdown, and at the end a full-screen finale card shows the
   totals. (Without `LOAD_TEST_START_GATE` the run starts by itself and the dashboard
   auto-detects it — the Start button then just dismisses the intro screen.)

5. **The official number**: when the terminal prints the final line, point at
   `submitted_parallel_tps=…` and `final_receipts_confirmed=12288` — the dashboard's
   figures are a live estimate (±1–2%); the terminal line is the measured, receipt-verified
   result.

## Troubleshooting

- **Port 8080 busy on the machine**: pick another (`LOAD_TEST_L2_RPC_PORT=9090`), re-tunnel
  with `-L 8080:localhost:9090` (the dashboard keeps using `localhost:8080` locally).
- **Dashboard stuck on *connection lost***: tunnel dropped — reconnect ssh; the page
  retries automatically.
- **Stale leftovers from a previous failed run**: `pkill -9 -f "deps/suite-"` and
  `rm -rf /tmp/.tmp*` on the machine, then check `df -h /` and `uptime` (load should be ~0).
- **Always rehearse once** before the meeting: same tunnel, same command. The first run
  after boot also warms the corpus generation cache.
