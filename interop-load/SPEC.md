# Interop Gateway Throughput Harness - Spec

Operational contract for the interop load test harness. The harness answers one
primary question:

> At what offered interop bundle rate does the gateway-mediated propagation path
> stop keeping up?

The measured path is:

```text
chain A sendBundle submitted
-> chain A sendBundle included
-> MessageRoot proof available
-> corresponding interop root imported on chain B
```

Destination `executeBundle` is optional. It is useful for end-to-end validation,
but it is not part of the core gateway throughput measurement.

Status: draft. Implementation to follow once this spec lands.

## 1. Scope

The harness exercises cross-chain interop on a local gateway setup with two
ZKsync OS chains and one gateway chain:

- chain A: source chain
- chain B: destination chain
- gateway: settlement layer for both chains

The harness measures throughput, latency, backlog growth, and recovery behavior
under sustained offered load. It assumes exclusive control of the RPC endpoints
it talks to and does not target mainnet, testnet, or shared environments.

Correctness testing remains the job of
`integration-tests/tests/protocol/interop.rs`. The harness may run a small smoke
flow against the provided RPC endpoints before load starts, but its primary
purpose is measurement, not semantic correctness coverage.

## 2. Flow

There is one load-test flow: `gateway-throughput`.

For each bundle:

1. Submit `InteropCenter.sendBundle` on chain A at the configured offered rate.
2. Wait for the source transaction receipt.
3. Poll `zks_getL2ToL1LogProof` with `LogProofTarget::MessageRoot` until the
   proof is available.
4. Extract `gateway_block_number` from the proof.
5. Poll chain B's `L2InteropRootStorage.interopRoots(gateway_chain_id,
   gateway_block_number)` until the root is imported.
6. Optionally call `executeBundle` on chain B if `--execute=true`.

The core throughput signal is the rate of successful `root_imported` events.
`executeBundle` is deliberately an optional tail so destination execution does
not hide the gateway propagation cliff.

## 3. CLI surface

```text
interop-load \
  --chain-a-rpc <url>             # required, e.g. http://127.0.0.1:3050
  --chain-b-rpc <url>             # required, e.g. http://127.0.0.1:3051
  --source-rpc <url>              # optional, repeatable; explicit source
                                  # lanes all target --chain-b-rpc by default
  --ring                          # optional; explicit source lanes send to
                                  # the next source lane, last to first
  --gateway-rpc <url>             # required, e.g. http://127.0.0.1:3052
  --l1-rpc <url>                  # required, e.g. http://127.0.0.1:8545
  --rich-privkey <hex>            # required, funds load wallets

  --duration <Xs|Xm|Xh>           # required, offered-load duration
  --rate <tx/s>                   # required, offered sendBundle rate
  --wallets <N>                   # required, number of source EOAs
  --seed <u64>                    # optional; default random and logged

  --strict-wallet-sizing          # default true; fail preflight if wallets
                                  # are below the assumed requirement
                                  # disable with --strict-wallet-sizing=false

  --rate-mode <open-loop|closed-loop> # default open-loop
  --max-in-flight <N>             # default 4 * rate; circuit breaker

  --calls-per-bundle <N>          # default 1
  --payload-bytes <N>             # default 32; payload per call
  --bundle-shape <message|asset-router> # default message

  --execute <true|false>          # default false; executeBundle on chain B
                                  # after root import when true
  --max-in-flight-execute <N>     # default 4 * rate; only used with execute

  --warmup <Xs|Xm>                # default 30s; metrics collected, wallet
                                  # sizing re-check emitted after warmup

  --output-dir <path>             # required; JSONL and summaries written here

  --skip-smoke-test               # optional; skips the one-bundle RPC smoke
                                  # flow and emits a warning event

  --metrics-url <url>             # optional, repeatable diagnostic endpoint.
                                  # If supplied, reachability is checked.

  --resilience <none|gateway-kill> # default none
  --gateway-kill-binary <path>    # required if resilience=gateway-kill;
                                  # external script that stops/restarts gateway
```

Exit codes:

- `0`: run completed and artifacts were written
- `1`: preflight failed
- `2`: run aborted due to fatal harness or RPC error
- `3`: invalid CLI args

Observed latency, throughput, open-loop violations, and bundle failures do not
change the exit code. They are evaluated from output artifacts.

## 4. Rate Model

Default rate mode is open-loop: the harness attempts to submit `sendBundle` at
the requested `--rate` regardless of observed latency. `--max-in-flight` is a
safety cap only.

The harness emits a per-second `throttle_tick` event:

- `requested_cumulative`: integral of `rate * elapsed` since load start
- `actual_cumulative`: submissions actually issued
- `submission_lag`: `requested_cumulative - actual_cumulative`
- `throttle_events_this_second`: submit attempts blocked by in-flight cap
- `inflight_at_tick`: current in-flight source submissions

A run is "pure open-loop" only if `submission_lag` stays bounded and no
throttle events occur. Otherwise `run_completed.open_loop_violated=true`.

Closed-loop mode is available for characterization, but it is not the default
because it hides the saturation cliff by backing off under pressure.

## 5. Wallet Sizing

Each source wallet owns its nonce stream. A wallet signs and submits the next
transaction only after the previous submission returns a tx hash; it does not
wait for inclusion. There is no shared nonce manager.

Default sizing assumption:

```text
assumed_p95_source_submission_pipeline = 10 seconds
required_wallets = ceil(rate * assumed_p95_source_submission_pipeline * 2)
```

The 10 second assumption covers source inclusion, proof availability, and root
import, because nonce pressure can surface anywhere in that pipeline. With
`--strict-wallet-sizing` enabled, preflight fails if `--wallets` is below this
requirement. Passing `--strict-wallet-sizing=false` allows the run to continue.

At the end of warmup, the harness recomputes the wallet requirement from
observed p95 `root_imported` latency and emits `wallet_sizing_warning` if the
configured wallet count is under-provisioned. It does not fail mid-run.

If a wallet hits a non-retriable submission error, the wallet is parked and a
`wallet_parked` event is emitted. Remaining wallets continue.

## 6. Backlog And Saturation Signals

The primary backlog is:

```text
source_included bundles without root_imported or bundle_failed
```

The harness reports backlog in every `second_tick` and in `summary.json`.

Gateway propagation is considered unable to keep up when one or more of these
conditions persist:

- `root_imported` throughput is lower than offered source inclusion throughput.
- backlog grows over multiple windows and does not drain after traffic stops.
- p95 or p99 `source_included -> root_imported` latency trends upward.
- open-loop submission lag grows due to in-flight cap pressure.
- source tx drops, proof timeouts, or root import timeouts increase.

The harness records these facts; it does not auto-tune rates or decide pass/fail
from them.

## 7. JSONL Schema

The harness writes one JSON object per line to `<output-dir>/events.jsonl`.
Every event has:

- `ts_ms`: unix milliseconds
- `event`: string discriminator
- `run_id`: uuid

Durations are milliseconds. Wei/token amounts are decimal strings.

### Lifecycle events

`run_started`

```json
{"ts_ms":1740000000000,"event":"run_started","run_id":"...",
 "config":{ "...":"full resolved CLI config" },
 "git_sha":"abc123","harness_version":"0.1.0"}
```

`preflight_passed`

```json
{"event":"preflight_passed","chain_a_id":6565,"chain_b_id":6566,
 "gateway_chain_id":506,"l1_chain_id":31337,
 "smoke_test_skipped":false,"metrics_enabled":false}
```

`smoke_test_skipped`

```json
{"event":"smoke_test_skipped","reason":"--skip-smoke-test"}
```

`warmup_completed`

```json
{"event":"warmup_completed","observed_root_import_p95_ms":7800,
 "required_wallets_from_observed_p95":780,"configured_wallets":1000}
```

`wallet_sizing_warning`

```json
{"event":"wallet_sizing_warning","observed_p95_ms":15300,
 "required_wallets":1530,"configured_wallets":1000}
```

`run_completed`

```json
{"event":"run_completed","duration_ms":1800000,
 "open_loop_violated":false,
 "totals":{"source_submitted":30000,"source_included":29998,
           "proof_available":29998,"root_imported":29998,
           "execute_submitted":0,"execute_included":0,
           "failed_classified":2},
 "final_backlog":0}
```

`run_aborted`

```json
{"event":"run_aborted","reason_class":"fatal_rpc_error",
 "reason_detail":"chain B eth_call failed repeatedly"}
```

### Per-bundle events

Bundles are identified by `bundle_id`, assigned before submission.

`source_submitted`

```json
{"event":"source_submitted","bundle_id":"...","wallet_idx":42,
 "calls_in_bundle":1,"payload_bytes":32,"bundle_shape":"message",
 "tx_hash":"0x..."}
```

`source_included`

```json
{"event":"source_included","bundle_id":"...","tx_hash":"0x...",
 "block_number":1234,"gas_used":150000,
 "latency_from_submit_ms":1800}
```

`proof_available`

```json
{"event":"proof_available","bundle_id":"...",
 "gateway_block_number":567,"latency_from_included_ms":3200,
 "latency_from_submit_ms":5000}
```

`root_imported`

```json
{"event":"root_imported","bundle_id":"...",
 "gateway_block_number":567,"destination_import_block":890,
 "latency_from_proof_available_ms":1500,
 "latency_from_included_ms":4700,
 "latency_from_submit_ms":6500}
```

`execute_submitted` (only when `--execute=true`)

```json
{"event":"execute_submitted","bundle_id":"...","wallet_idx":17,
 "tx_hash":"0x..."}
```

`execute_included` (only when `--execute=true`)

```json
{"event":"execute_included","bundle_id":"...","tx_hash":"0x...",
 "block_number":2345,"gas_used":280000,"success":true,
 "latency_from_execute_submit_ms":800,
 "end_to_end_latency_ms":9300}
```

`bundle_failed`

```json
{"event":"bundle_failed","bundle_id":"...","stage":"root_import",
 "reason_class":"root_import_timeout",
 "reason_detail":"root not imported within 300000ms","tx_hash":"0x..."}
```

Defined `reason_class` values:

- `submit_rpc_error`: source RPC rejected submission
- `source_tx_dropped`: source tx hash not mined before timeout
- `proof_timeout`: proof unavailable before timeout
- `root_import_timeout`: root not imported on chain B before timeout
- `execute_rpc_error`: execute submission rejected
- `execution_reverted`: execute tx mined but reverted
- `wallet_parked`: wallet permanently dropped due to RPC error

### Per-second events

`throttle_tick`

```json
{"event":"throttle_tick","lane":"source",
 "requested_cumulative":5000,"actual_cumulative":4980,
 "submission_lag":20,"throttle_events_this_second":0,
 "inflight_at_tick":47}
```

If `--execute=true`, the harness may also emit `lane:"execute"` ticks for the
optional destination execution tail.

`second_tick`

```json
{"event":"second_tick",
 "source_submitted_this_second":50,
 "source_included_this_second":49,
 "proof_available_this_second":48,
 "root_imported_this_second":48,
 "execute_submitted_this_second":0,
 "execute_included_this_second":0,
 "in_flight_source":95,
 "in_flight_execute":0,
 "gateway_backlog":47}
```

### Resilience events

`gateway_kill_started`

```json
{"event":"gateway_kill_started","at_run_elapsed_ms":900000}
```

`gateway_recovered`

```json
{"event":"gateway_recovered","kill_duration_ms":30200,
 "first_post_restart_root_at_ms":932000,
 "first_post_restart_root_latency_ms":1800,
 "backlog_at_kill":412,
 "backlog_drained_at_ms":970000,
 "backlog_drain_duration_ms":38000,
 "permanently_stuck_bundles":0}
```

A bundle is permanently stuck if it was source-included before the kill, not
root-imported by run end, and run end is at least 60 seconds after
`gateway_recovered`.

## 8. Preflight Checklist

Run before load starts. Failure exits with code 1.

1. RPC reachability: call `eth_chainId` against chain A, chain B, gateway, and
   L1 RPCs. Record chain IDs.
2. Harness smoke flow: unless `--skip-smoke-test` is set, submit one real
   bundle through the provided RPCs and wait for `root_imported`. If
   `--execute=true`, also execute it on chain B.
3. Optional metrics reachability: for every supplied `--metrics-url`, perform a
   simple HTTP reachability check and record the result. Metrics are diagnostic
   only and are not required for v1.
4. Rich account balance: confirm `--rich-privkey` has enough L1 balance to fund
   load wallets at the configured allowance.
5. Wallet sizing: apply section 5. With strict sizing on, fail if
   under-provisioned.

## 9. Recommended Run Matrix

The harness exposes knobs; this matrix is operational guidance, not a hardcoded
suite.

| # | Name | Execute | Rate | Wallets | Duration | Notes |
|---|------|---------|------|---------|----------|-------|
| 1 | Smoke | false | 1 tx/s | 20 | 5m | Confirms source->gateway->destination root import. |
| 2 | Baseline characterization | false | 10 tx/s | scaled | 15m | No numeric SLO; classify failures and latency shape. |
| 3 | Gateway throughput ramp | false | 10 -> 25 -> 50 -> 100 -> 200 tx/s | scaled | 10m each | Stop when backlog grows without drain or p95 trends up. |
| 4 | Large bundle ramp | false | 5 -> 10 -> 25 tx/s | scaled | 10m each | `--calls-per-bundle 10 --bundle-shape asset-router`. |
| 5 | End-to-end validation | true | 70% of #3 cliff | scaled | 30m | Confirms optional execute path does not reveal hidden correctness failures. |
| 6 | Soak | false | 70% of #3 cliff | scaled | 60-120m | Acceptance: no crash, no permanent root backlog, all failures classified. |
| 7 | Gateway-kill soak | false | 70% of #3 cliff | scaled | 60m | `--resilience gateway-kill`; backlog must drain after restart. |

Scale wallets per section 5 against the highest rate in the run.

## 10. Initial Acceptance Criteria

First runs characterize the system. They do not fail because a numeric SLO was
missed.

A run is acceptable if:

1. No node process crashes outside intentional resilience actions.
2. After traffic stops, every source-included bundle has either `root_imported`
   or `bundle_failed`.
3. `final_backlog` is zero after the post-load drain window, unless the run is
   explicitly being used to capture a saturation cliff.
4. Every `bundle_failed` has a classified `reason_class`.
5. If `--execute=true`, successfully executed bundles reconcile against
   destination state in `<output-dir>/reconciliation.json`.
6. `<output-dir>/summary.json` is written.

Numeric throughput and latency targets should be set only after baseline and
ramp runs have shown the actual cliff.

## 11. Output Artifacts

After every run, `<output-dir>` contains:

- `events.jsonl`: primary event stream
- `summary.json`: aggregate counters, p50/p95/p99 per stage, backlog stats,
  throttle totals, and `open_loop_violated`
- `config.json`: fully resolved CLI config including defaults and generated
  seed if none was supplied
- `preflight.json`: preflight step results
- `reconciliation.json`: only when `--execute=true`

A separate post-processor, `interop-load-summarize`, may consume `events.jsonl`
and produce CSV views. CSV is not a primary artifact.

## 12. Non-goals

- Destination execution saturation as an isolated benchmark. If needed later,
  build a separate prewarmed execute harness.
- Gateway-only internal root driver. This harness drives public chain RPC flows,
  not gateway internals.
- Mainnet, testnet, or shared environment testing.
- Multi-destination or more-than-two-chain topology.
- Correctness coverage beyond the smoke flow.
- Auto-tuning rates based on observed latency.
