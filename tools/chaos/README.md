# Chaos rig

Runs a containerized BFT validator committee and injects faults on a seeded random
schedule — kill, graceful stop, freeze (container pause), network partition — for as
long as you leave it running. The goal is to surface the bugs that only sustained,
randomized wall-clock time finds: torn state after dirty crashes, teardown races,
drift interactions, slow leaks.

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

## Notes and known gaps

- **anvil image**: the compose file pins `ghcr.io/foundry-rs/foundry:v1.5.1` (newer
  anvil cannot load the checked-in L1 state). If the image tag or its entrypoint
  differs on your host, adjust the `anvil` service — the command just gunzips the
  chain's `l1-state.json.gz` and runs anvil the same way `run_local.sh` does.
- **Load generation** is not wired yet: the chain runs on empty-block cadence, which
  exercises every consensus/restart/liveness invariant but no transaction flow. To
  add load, fund an L2 account (a deposit through the bridgehub) and run `loadbase`
  against any validator's RPC port. Wiring a funded spammer into the rig is a
  follow-up.
- **The invariant monitor** (cross-node hash agreement, liveness-when-expected,
  rejoin deadlines, log scanning, artifact capture, alerting) is the rig's second
  half and lives in a follow-up; until then the journal plus `/status` polling and
  grafana are the eyes.
