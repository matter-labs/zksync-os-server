# Live-Update Test

Verifies that the current build can continue operating from a database produced by
the previous released version running against a live cluster.

## What it does

**Phase 1 — old binary:** Downloads a DB snapshot from a running Kubernetes sequencer pod,
spins up a forked Anvil L1 at the current L1 tip, and runs the old server binary (downloaded
from GitHub releases based on the pod's image tag) against that DB until it produces at least
3 new L2 blocks.

**Phase 2 — new binary:** Stops the old server, then starts the current build in-process
against the same DB and waits for it to produce 3 more L2 blocks. If it does, the upgrade
is considered compatible.

## Prerequisites

- **`kubectl`/`KUBECONFIG`**: access to the cluster (needed on every run to check the image
  tag and fetch operator keys — see [Cache behaviour](#cache-behaviour) below)
- **`anvil`** (Foundry): must be in `PATH`
- A publicly accessible L1 RPC URL that can serve the real L1 (e.g. Infura/Alchemy Sepolia)

## Required environment variables

| Variable | Example | Description |
|---|---|---|
| `LIVE_UPDATE_NAMESPACE` | `testnet-alpha` | Kubernetes namespace of the sequencer pod |
| `LIVE_UPDATE_POD` | `sequencer-c-0` | Kubernetes pod name |
| `LIVE_UPDATE_L1_RPC_URL` | `https://sepolia.infura.io/v3/<key>` | Publicly accessible L1 RPC (Sepolia) |

## Optional environment variables

| Variable | Default | Description |
|---|---|---|
| `LIVE_UPDATE_ARTIFACTS_DIR` | `<workspace>/live-update-cache/<ns>/<pod>/` | Override cache location |
| `LIVE_UPDATE_OLD_BIN` | *(auto-download from GitHub)* | Path to a local old binary — skips GitHub download |
| `LIVE_UPDATE_L2_WALLET_SK` | *(no transactions sent)* | Hex private key of a funded L2 account to send transactions and accelerate batch sealing |

## How to run

```bash
LIVE_UPDATE_NAMESPACE=testnet-alpha \
LIVE_UPDATE_POD=sequencer-c-0 \
LIVE_UPDATE_L1_RPC_URL=https://sepolia.infura.io/v3/<key> \
cargo nextest run -p zksync_os_integration_tests \
  --features live-update \
  node::live_update \
  --include-ignored
```

On the first run this downloads the DB snapshot, genesis.json, config.yaml, and old server
binary. Depending on the size of the production DB, the snapshot download can take several
minutes. Subsequent runs reuse the cache (see below).

## Cache behaviour

Artifacts are cached at `<workspace>/live-update-cache/<namespace>/<pod>/`:

```
live-update-cache/
└── testnet-alpha/
    └── sequencer-c-0/
        ├── db/               # Pristine RocksDB snapshot (never modified)
        ├── genesis.json
        ├── config.yaml
        ├── old-server        # Cached binary
        ├── image-tag         # Cache key: pod container image tag
        └── runs/
            └── <timestamp>/
                ├── db/       # Per-run working copy (separate from pristine)
                └── logs/
                    ├── anvil.log
                    └── old-server.log
```

**On every run**, the test reads the current pod image tag from Kubernetes and fetches
operator keys from the `sequencer` secret (keys are never cached for security reasons).

**Cache is reused** as long as the pod's image tag is unchanged. No DB snapshot or binary
download happens. A typical cached run only hits the cluster for the image tag check and
operator keys.

**Cache is invalidated** (wiped and re-downloaded) when the pod's container image tag
changes — e.g., after a cluster upgrade.

**To force a re-download**, delete the cache directory:
```bash
rm -rf live-update-cache/testnet-alpha/sequencer-c-0
```

## Using a local binary (skip GitHub download)

If you already have a built binary (e.g., from a local checkout of the previous release),
point `LIVE_UPDATE_OLD_BIN` at it:

```bash
LIVE_UPDATE_OLD_BIN=/path/to/old/zksync-os-server \
LIVE_UPDATE_NAMESPACE=testnet-alpha \
LIVE_UPDATE_POD=sequencer-c-0 \
LIVE_UPDATE_L1_RPC_URL=https://... \
cargo nextest run -p zksync_os_integration_tests \
  --features live-update node::live_update --include-ignored
```

The DB snapshot, genesis, and config are still cached normally.

## Finding logs after a run

Each run creates a timestamped directory under the cache:

```
live-update-cache/testnet-alpha/sequencer-c-0/runs/<timestamp>/logs/
  anvil.log        # Forked Anvil output
  old-server.log   # Old binary stdout + stderr
```

Run directories are never deleted automatically, so you can inspect them after failures.

## Troubleshooting

**Test hangs waiting for L2 blocks**
- Check `old-server.log` / test output for errors from the old server
- Ensure `LIVE_UPDATE_L1_RPC_URL` is reachable from your machine (Anvil must be able to fork it)
- Try setting `LIVE_UPDATE_L2_WALLET_SK` to a funded L2 account to send transactions and trigger batch sealing

**`failed to create Kubernetes client`**
- Check that `KUBECONFIG` is set and `kubectl get pod -n $LIVE_UPDATE_NAMESPACE $LIVE_UPDATE_POD` works

**`failed to spawn anvil`**
- Install Foundry: https://getfoundry.sh

**`server returned error status` for binary download**
- The image tag may not correspond to a GitHub release (e.g., a dev/staging build)
- Use `LIVE_UPDATE_OLD_BIN` to point to a local binary instead

**Snapshot script failed inside pod**
- Check the pod has `/db` with the expected layout
- The error message includes the script's stderr output
