# Chaos rig

Runs a containerized BFT validator committee and injects faults on a seeded random
schedule — kill, graceful stop, freeze (container pause), network partition — for as
long as you leave it running, while a built-in watcher continuously checks that the
consensus algorithm's properties hold. The goal is to surface the bugs that only
sustained, randomized wall-clock time finds: torn state after dirty crashes,
teardown races, drift interactions, slow leaks.

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
- **monotone finality** — no validator's finalized view or applied height ever goes
  backwards;
- **no progress without quorum** — while the driver holds the healthy set below
  quorum, the finalized tip must freeze (after `--settle-margin`, default 5s, for
  certificates already in flight);
- **no protocol fault evidence** — the `conflicting_*`/`nullify_finalize` activity
  counters must stay zero on an honest committee;
- **no unexpected deaths** — a container the driver did not touch must be running;
- **clean logs** — no panics or ERROR lines beyond an explicit allowlist of known
  teardown noise;
- **liveness** — when the committee is expected live for a whole `--liveness-window`
  (default 60s, deliberately generous), the finalized view must advance within it.

On the first finding the experiment freezes: injection stops, nothing is healed, the
findings plus the offending poll plus every container's recent logs land in
`<workdir>/artifacts/`, and the driver exits nonzero. The cluster stays up exactly
as it failed — attach, inspect, then `docker compose down -v` when done.

## Notes and known gaps

- **anvil image**: the compose file pins `ghcr.io/foundry-rs/foundry:v1.5.1` (newer
  anvil cannot load the checked-in L1 state). The service gunzips the chain's
  `l1-state.json.gz` and runs anvil the same way `run_local.sh` does.
- **Static validator IPs**: the node parses committee addresses as socket addresses
  (numeric, no DNS), so `setup` pins each validator's IP on the compose network and
  a partition heal reconnects with `--ip` to restore exactly the address the rest of
  the committee dials.
- **Load generation** is not wired yet: the chain runs on empty-block cadence, which
  exercises every consensus/restart/liveness invariant but no transaction flow. To
  add load, fund an L2 account (a deposit through the bridgehub) and run `loadbase`
  against any validator's RPC port. Wiring a funded spammer into the rig is a
  follow-up.
