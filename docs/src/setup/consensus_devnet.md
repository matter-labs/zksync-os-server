# Local consensus devnet

Runs a multi-validator BFT committee on your machine: N full nodes in containers over
one local L1 (anvil), producing, verifying, and finalizing real blocks. Use it to
develop against a consensus-enabled chain — the same tooling also powers the chaos
rig (`tools/chaos`), so a devnet can be upgraded into a fault-injection soak at any
time.

For what consensus is, how it is integrated, and how real chains enable it, see the
[Consensus](../consensus/index.md) section.

## Prerequisites

- Docker (on Docker Desktop, give the VM ≥ 8 GiB of memory).
- A Rust toolchain (the generator is a workspace tool).

## Bring-up

From the repository root:

```sh
# 1. Build the node image. On a memory-constrained Docker VM, bound the build
#    parallelism or the compile stage gets OOM-killed:
docker build -t zksync-os-server:latest --build-arg CARGO_BUILD_JOBS=4 .

# 2. Generate the cluster: committee keys, one config overlay per validator, a
#    compose file, and a manifest with every mapped port.
cargo run -p zksync_os_chaos -- setup --validators 4 --out ./devnet --repo .

# 3. Start it (anvil + validators).
docker compose -f ./devnet/docker-compose.yaml up -d
```

The cluster is up once every validator answers `/status` with a `consensus` section —
committee size, this validator's identity, and the latest finalized consensus round:

```sh
curl -s localhost:<status-port>/status | jq .consensus
```

Host ports for each validator (L2 RPC, status, prometheus) are listed in
`./devnet/manifest.json`. Any validator's RPC accepts transactions: validators gossip
pending transactions to each other, so a transaction submitted to one node rides a
block regardless of which validator leads next.

## Day-to-day

- **Logs**: `docker compose -f ./devnet/docker-compose.yaml logs -f validator-1`
- **Restart a validator** (it rejoins and catches up on its own volume):
  `docker restart chaos-validator-1`
- **Load**: `cargo run -p zksync_os_chaos -- load --workdir ./devnet --tps 10 --pattern sustained --spread even --duration 10m`
  funds senders through real L1 deposits and streams transfers through the committee.
- **Faults**: the same workdir drives the chaos rig —
  `cargo run -p zksync_os_chaos -- drive --workdir ./devnet --seed 1 --fault-interval 30s`
  (see `tools/chaos/README.md` for the watcher and its checks).

## Reset

```sh
docker compose -f ./devnet/docker-compose.yaml down -v
```

Volumes hold each validator's chain data; `down -v` deletes them for a fresh chain.
Without `-v`, a stopped devnet resumes where it left off.

## Scope

One machine, one compose network, settlement to the local anvil. It exercises the
full consensus path (leadership rotation, verification-before-vote, finalization,
settlement by the batcher validator) but is not a performance environment — latency
and throughput numbers only mean something on a real deployment.
