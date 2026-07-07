# Chaos rig

Runs a containerized BFT validator committee and injects faults on a seeded random
schedule — kill, graceful stop, freeze (container pause), network partition, and
network degradation (packet loss + delay via `tc netem`) — for as long as you leave
it running, while a built-in watcher continuously checks that the consensus
algorithm's properties hold and `chaos load` puts real transaction traffic through
the committee. The goal is to surface the bugs that only sustained, randomized
wall-clock time finds: torn state after dirty crashes, teardown races, drift
interactions, slow leaks.

Ground rules:

- **Telescope, not gate.** Nothing in CI depends on this. Findings are triaged by a
  human and distilled into deterministic regression tests (simulation scenarios or
  integration tests) where they belong.
- **The driver always knows what it broke.** It never takes the healthy set below
  quorum except through a deliberate, bounded outage window, and it journals the
  expected liveness after every action — so a monitor can tell an injected outage
  from a real one.
- **Seeds replay the experiment, not the execution.** The schedule is deterministic
  per seed; the system's response is not. Capture artifacts (journal, logs, volumes)
  when something looks wrong.

## Bring-up

Everything below runs from the repository root on a machine with docker.

```sh
# 1. Build the node image (the production Dockerfile).
docker build -t zksync-os-server:latest .

# 2. Generate a cluster work directory: keys, per-validator configs, compose file.
cargo run -p zksync_os_chaos -- setup --validators 5 --out ./chaos-workdir --repo .

# 3. Bring the cluster up (anvil + validators; ports per chaos-workdir/manifest.json).
docker compose -f ./chaos-workdir/docker-compose.yaml up -d

# 4. Drive faults against it.
cargo run -p zksync_os_chaos -- drive --workdir ./chaos-workdir --seed 42 \
    --fault-interval 30s            # add --duration 2h for a bounded run

# 5. (Optionally, in parallel:) put transaction load on the committee.
cargo run -p zksync_os_chaos -- load --workdir ./chaos-workdir \
    --profile realistic --duration 2h
```

Watch the cluster through any validator's mapped ports (see the manifest):
`/status` for consensus progress, `/status/consensus-metrics` plus the prometheus
port for scraping. `docker compose logs -f validator-2` for one node's logs.

The driver heals everything (restarts, unpauses, reconnects) on exit; `docker compose
down -v` resets the world, and volumes persist state across restarts otherwise — a
restarted validator rejoins on its own history, which is the point.

## The watcher

`chaos drive` runs a watcher alongside the fault schedule. Every couple of seconds
it polls each validator (container state, `/status`, one `eth_getBlockByNumber`,
the consensus metrics, new log lines) and checks, cross-referenced against what the
driver injected:

- **agreement** — every reachable validator serves the identical block hash at the
  probed height;
- **execution agreement** — matching hashes are necessary, not sufficient: every
  reachable validator must also serve the same transaction list for the probed
  block and the same receipts (status, gas used, logs bloom) for a deterministic
  sample of its transactions — the tripwire for RPC/storage-layer divergence;
- **no verify rejections** — the `consensus_verify_verdicts{verdict="invalid"}`
  counter must never tick on an honest cluster: a validator permanently rejecting
  a peer's proposal (failed linkage, validity, or re-execution mismatch) is the
  operational signature of an STF divergence;
- **monotone finality** — no validator's finalized view or applied height ever goes
  backwards;
- **no progress without quorum** — while the driver holds the healthy set below
  quorum, the finalized tip must freeze (after `--settle-margin`, default 5s, for
  certificates already in flight);
- **no protocol fault evidence** — the `conflicting_*`/`nullify_finalize` activity
  counters must stay zero on an honest committee;
- **no unexpected deaths** — a container the driver did not touch (or has merely
  paused, partitioned, or degraded) must be running;
- **clean logs** — no panics or ERROR lines beyond an explicit allowlist of known
  teardown noise;
- **liveness** — when the committee is expected live for a whole `--liveness-window`
  (default 60s, deliberately generous), the finalized view must advance within it;
  a stall finding names its laggards (validators whose own finalized round sits
  below the stalled tip) so triage starts with the right node.

On the first finding the experiment freezes: injection stops, nothing is healed, the
findings plus the offending poll plus every container's recent logs land in
`<workdir>/artifacts/`, and the driver exits nonzero. The cluster stays up exactly
as it failed — attach, inspect, then `docker compose down -v` when done.

## The L1 lane

`chaos drive` also faults the L1 itself (disable with `--no-l1-faults`):

- **L1 blackout** — the anvil container is paused for a bounded window. L2
  consensus is expected to keep finalizing right through it (the watcher keeps
  checking liveness); L1-facing components may log connectivity errors, and
  exactly those — transport-shaped ERROR lines — are tolerated while the
  blackout holds, so a panic or an unrelated ERROR still trips the log check.
- **Base-fee spikes** — `anvil_setNextBlockBaseFeePerGas` multiplies the L1
  base fee (5–40×); EIP-1559 decay on anvil's mostly-empty blocks walks it
  back down. Exercises fee-tracking paths.
- **Shallow reorgs** (`--l1-reorgs`, off by default) — `anvil_reorg` replaces
  the last few unfinalized L1 blocks. A findings-only probe of l1_watcher
  assumptions: the node may simply not be ready for reorgs, which is why this
  is opt-in and trivial to leave off.

## Network degradation

Besides binary faults, the schedule degrades validators' networking in place with
`tc netem` (`docker exec`, root, `NET_ADMIN` from the compose file): packet loss
plus delay with jitter, from a conservative menu (5%/50ms up to 30%/300ms).
Degraded validators still **count as live** — a lossy, slow network is exactly the
weather consensus must ride through, so a stall while nodes are merely degraded is
a finding, not an excuse. Degradation applies to the container's whole interface
(peers and L1 alike); per-destination shaping (peers-only vs L1-only, via tc
filters) is a known possible extension.

## Load

`chaos load` puts real transactions through the committee while the driver does
its work. Sender accounts are derived deterministically and funded once through a
real L1→L2 bridgehub deposit (signed by anvil's default rich account); a
**profile** then decides what they send.

A profile (`--profile <name-or-path>`, TOML) sets the rate, the traffic shape,
and the workload mix. Built-ins under `tools/chaos/profiles/` double as starting
points for hand-rolled mixes:

- `default` — plain transfers only (the original behavior);
- `realistic` — the staple soak mix: mostly boring traffic plus every
  STF-coverage workload at a sensible weight;
- `guzzler` — expensive blocks on purpose (compute burn + calldata bulk);
- `quiet` — a low background murmur in bursts;
- `smoke` — everything at equal weight, for short development runs.

The tick-driven workloads (weights in `[weights]`): `transfers` (1 wei to fresh
addresses), `erc20` (mint/transfer/approve churn), `call_maze` (seeded walks
through nested CALL/DELEGATECALL/STATICCALL with CREATE/CREATE2 leaves and
bubbling reverts), `precompiles` (known-vector exercises, self-calibrating to
what the chain's VM supports), `context_probe` (the environment-opcode family
emitted into logs), `gas_guzzler` (burns nearly a whole gas limit per
transaction), `failing` (transactions *meant* to revert or run out of gas),
`blobs` (big random calldata). Sagas (`[sagas]`) run beside the tick loop with
their own cadence and assertions: `nonce_race` signs two different transactions
with the same nonce and submits them to two validators' mempools at once —
exactly one may ever mine. Contract-based workloads deploy their contracts on
first use (foundry project under `tools/chaos/contracts/`, built by `build.rs`
when forge is installed) and reuse live deployments across runs.

L1-flow sagas ride the same `[sagas]` table: `deposits` (a trickle of real
priority operations with occasional bursts, asserting L2 arrival), `withdrawals`
(the full round trip, continuously: L2 withdraw → batch executed on L1 → log
proof → `L1Nullifier.finalizeDeposit` → exact L1 balance), and
`failed_deposits` (deposits engineered to revert or run out of gas on L2 —
asserting only what must always hold: the relay includes them and the priority
queue keeps working afterwards, while *recording* the observed refund
semantics in the report, since no other test in the repo exercises that path
yet). Each L1 saga runs on its own funded L1 account so none of them race
another's nonces.

Flags override the profile's shape knobs: `--tps N`, `--pattern
sustained|bursts`, `--burst-secs`/`--idle-secs`, `--spread even|single:<i>`.

Submission failures are counted, never fatal — validators go down mid-run by
design. The end-of-run report shows per-validator and per-workload counts, saga
verdicts, whether each sender's final transaction was included, and an
**expectation audit**: a sample of receipts checked against each workload's
declared expectation (clean traffic landed with status 1, planned failures with
status 0). Audit violations and saga failures exit nonzero.

## Notes and known gaps

- **anvil image**: the compose file pins `ghcr.io/foundry-rs/foundry:v1.5.1` (newer
  anvil cannot load the checked-in L1 state). The service gunzips the chain's
  `l1-state.json.gz` and runs anvil the same way `run_local.sh` does; it is also
  published on a host port (see the manifest) so `chaos load` can fund deposits.
- **Static validator IPs**: the node parses committee addresses as socket addresses
  (numeric, no DNS), so `setup` pins each validator's IP on the compose network and
  a partition heal reconnects with `--ip` to restore exactly the address the rest of
  the committee dials.
- **Log rotation**: container logs are capped (json-file, 50 MB × 3) so the
  watcher's `docker logs` polls stay cheap on multi-hour runs.
