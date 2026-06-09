# interop-load

Load harness for measuring gateway-mediated interop throughput on a local
ZKsync OS gateway setup.

The v1 target path is:

```text
chain A sendBundle submitted
-> chain A sendBundle included
-> MessageRoot proof available
-> interop root imported on chain B
```

See [SPEC.md](./SPEC.md) for the full operational contract.

## Quickstart

Start the local gateway topology from the repository root:

```bash
./run_local.sh ./local-chains/v31.0/multi_chain --logs-dir ./logs/interop-load
```

Then run the message-bundle gateway propagation path from the repository root:

```bash
cargo run -p interop-load -- \
  --chain-a-rpc http://127.0.0.1:3050 \
  --chain-b-rpc http://127.0.0.1:3051 \
  --gateway-rpc http://127.0.0.1:3052 \
  --l1-rpc http://127.0.0.1:8545 \
  --rich-privkey 0x0000000000000000000000000000000000000000000000000000000000000001 \
  --duration 30s \
  --rate 1 \
  --wallets 20 \
  --wallet-fund-wei 1000000000000000000 \
  --output-dir ./logs/interop-load/run-001
```

For a 5-chain ring saturation run, pass all five chain RPCs as repeated
`--source-rpc` flags and add `--ring`. The aggregate `--rate` is striped
round-robin across those chains; each chain sends to the next one in the list,
and the last chain sends to the first:

```bash
target/release/interop-load \
  --source-rpc http://127.0.0.1:3050 \
  --source-rpc http://127.0.0.1:3051 \
  --source-rpc http://127.0.0.1:3053 \
  --source-rpc http://127.0.0.1:3054 \
  --source-rpc http://127.0.0.1:3055 \
  --ring \
  --chain-a-rpc http://127.0.0.1:3050 \
  --chain-b-rpc http://127.0.0.1:3051 \
  --gateway-rpc http://127.0.0.1:3052 \
  --l1-rpc http://127.0.0.1:8545 \
  --rich-privkey "$RICH_PRIVKEY" \
  --setup ./logs/interop-load-setup.json \
  --duration 5m \
  --warmup 30s \
  --rate 500 \
  --wallets 1000 \
  --output-dir ./logs/interop-load/5chain-ring-500tps \
  --skip-smoke-test
```

With this topology, `root_imported_per_sec` is the aggregate Gateway propagation
throughput across lanes `6565→6566`, `6566→6567`, `6567→6568`, `6568→6569`,
and `6569→6565`. Use `--rate 500` for 100 TPS offered per chain.

Current implementation status: the crate can drive the message-bundle path from
`sendBundle` submission through MessageRoot proof availability and root import on
chain B. `--bundle-shape asset-router` and `--execute true` are explicit
unsupported modes for now.

## Running the 5-chain interop load test

The `local-chains/v31.0_5chain/multi_chain` setup boots a gateway (`506`) plus
five child chains (`6565`–`6569`) wired into a ring:
`6565→6566→6567→6568→6569→6565`. It is a self-contained ecosystem, separate from
`local-chains/v31.0` (which the integration-test suite depends on). For the
chain-setup details (ports, DA modes, how to regenerate the L1 state), see
[local-chains/v31.0_5chain/multi_chain/README.md](../local-chains/v31.0_5chain/multi_chain/README.md).

Both procedures below run the harness identically — the only difference is which
L1 state / genesis the stack boots from (see the linked setup README). The
harness measures source→destination interop latency and writes percentiles to
`summary.json` (and a one-line summary to stderr). 20 TPS per chain over five
chains is an aggregate `--rate 100`.

### 1. Load test on 5 chains *without* validiums (all rollup)

The committed `v31.0_5chain` state is **mixed** (3 rollup + 2 validium). An
all-rollup state is **not committed** and must be regenerated first — see
[Regenerating the L1 state](../local-chains/v31.0_5chain/multi_chain/README.md#regenerating-the-l1-state)
in the setup README (set `validium_user_chains=()`). Once the all-rollup
`l1-state.json.gz` / `genesis.json` are in place:

```bash
# Terminal 1 — boot gateway + five all-rollup child chains + Anvil
./run_local.sh ./local-chains/v31.0_5chain/multi_chain --logs-dir ./logs/rollup-5chain/stack

# Terminal 2 — drive 20 TPS/chain (100 aggregate) around the ring
target/release/interop-load \
  --source-rpc http://127.0.0.1:3050 \
  --source-rpc http://127.0.0.1:3051 \
  --source-rpc http://127.0.0.1:3053 \
  --source-rpc http://127.0.0.1:3054 \
  --source-rpc http://127.0.0.1:3055 \
  --ring \
  --chain-a-rpc http://127.0.0.1:3050 \
  --chain-b-rpc http://127.0.0.1:3051 \
  --gateway-rpc http://127.0.0.1:3052 \
  --l1-rpc http://127.0.0.1:8545 \
  --rich-privkey 0x0000000000000000000000000000000000000000000000000000000000000001 \
  --setup ./logs/interop-load-setup.json \
  --duration 5m \
  --warmup 30s \
  --rate 100 \
  --wallets 1000 \
  --output-dir ./logs/rollup-5chain/load \
  --skip-smoke-test
```

### 2. Load test *with* validium chains (mixed rollup + validium)

This is the committed setup as-is — no regeneration needed. Chains `6568` and
`6569` are validiums; `6565`–`6567` are rollups. The harness invocation is
identical to case 1; only the chain stack differs:

```bash
# Terminal 1 — boot the committed mixed (rollup + validium) setup
./run_local.sh ./local-chains/v31.0_5chain/multi_chain --logs-dir ./logs/validium-5chain/stack

# Terminal 2 — same harness invocation, different output dir
target/release/interop-load \
  --source-rpc http://127.0.0.1:3050 \
  --source-rpc http://127.0.0.1:3051 \
  --source-rpc http://127.0.0.1:3053 \
  --source-rpc http://127.0.0.1:3054 \
  --source-rpc http://127.0.0.1:3055 \
  --ring \
  --chain-a-rpc http://127.0.0.1:3050 \
  --chain-b-rpc http://127.0.0.1:3051 \
  --gateway-rpc http://127.0.0.1:3052 \
  --l1-rpc http://127.0.0.1:8545 \
  --rich-privkey 0x0000000000000000000000000000000000000000000000000000000000000001 \
  --setup ./logs/interop-load-setup.json \
  --duration 5m \
  --warmup 30s \
  --rate 100 \
  --wallets 1000 \
  --output-dir ./logs/validium-5chain/load \
  --skip-smoke-test
```

### Reading the latency result

`summary.json` contains a `latency` object with end-to-end and per-stage
percentiles (`source_submitted` → `root_imported`), aggregate and per-lane:

```jsonc
"latency": {
  "aggregate": {
    "end_to_end":       { "count": 2690, "p50_ms": 8000, "p95_ms": 12900, "p99_ms": 16100, ... },
    "submit_to_included": { ... },   // source-chain inclusion
    "included_to_proof":  { ... },   // batch seal + gateway MessageRoot proof
    "proof_to_root":      { ... }    // root import on the destination chain
  },
  "per_lane": [ { "source_chain_id": 6565, "destination_chain_id": 6566, ... }, ... ]
}
```

`end_to_end` is the "time for an interop tx to reach the destination chain"
metric. Only measured bundles (after `--warmup`) that actually reached
`root_imported` contribute samples.

### Port reference

| Role        | Chain | Port |
|-------------|-------|------|
| source A    | 6565  | 3050 |
| dest / src  | 6566  | 3051 |
| gateway     | 506   | 3052 |
| source      | 6567  | 3053 |
| validium    | 6568  | 3054 |
| validium    | 6569  | 3055 |
| L1 (Anvil)  | —     | 8545 |
