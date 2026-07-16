---
name: generate-zksync-local-state
description: Generate and verify DB-free, single-chain ZKsync OS local-chain fixtures with zk-deployer for arbitrary protocol versions and era-contracts Git revisions. Use when creating or regenerating local-chains/v31, v32, or later fixture directories; changing the contracts revision or chain ID; producing compact Anvil l1-state.json.gz snapshots; or replacing Gateway/multi-chain fixtures with a direct-L1 single-chain setup.
---

# Generate ZKsync Local State

Generate a self-consistent local fixture from a selected era-contracts commit. Keep the L1
snapshot compact by mining only submitted transactions during deployment. Do not package a
node database or `contracts.yaml`.

## Gather inputs

Determine:

- ZKsync OS server repository root.
- era-contracts commit hash. Require the user to provide or confirm this value.
- Protocol version such as `v31`, `v31.0`, or `v32`.
- L2 chain ID; default to `506` only when the user does not specify one.
- zk-deployer repository and revision. Default to the sibling
  `zksync-os-integration-tests` checkout at `HEAD`; select a compatible revision if the chosen
  contracts commit does not compile with it.

Read the current zk-deployer README before generation because its intent schema and commands
may evolve:

`<zk-deployer-repo>/bin/zk-deployer/README.md`

Inspect repository instructions and existing worktree changes before writing. Preserve
unrelated changes.

## Generate

Run the bundled generator:

```bash
REPO_ROOT=$(git rev-parse --show-toplevel)
"$REPO_ROOT/.agents/skills/generate-zksync-local-state/scripts/generate-local-state.sh" \
  --server-root /path/to/zksync-os-server \
  --zk-deployer-repo /path/to/zksync-os-integration-tests \
  --contracts-rev <era-contracts-commit> \
  --protocol-version v32 \
  --chain-id 506
```

The script:

1. Creates a detached temporary worktree for zk-deployer.
2. Pins both era-contracts Rust dependencies to the requested commit.
3. Builds zk-deployer and contract artifacts.
4. Deploys one direct-L1 chain against an external Anvil with no block interval. Anvil's
   normal transaction automining remains enabled, so every block in the saved history contains
   a deployment or funding transaction.
5. Runs `bootstrap --broadcast`, `apply --broadcast`, and `server-config`.
6. Asserts that the historical-state count equals the transaction count and that the snapshot
   contains no idle blocks.
7. Writes `l1-state.json.gz`, `genesis.json`, `versions.yaml`, `default/config.yaml`,
   `default/wallets.yaml`, and `default/README.md`.

The script refuses to overwrite an existing version directory. Inspect the target and obtain
authorization before using `--force`; it replaces that entire version directory. Use
`--output-dir /tmp/...` for a non-destructive trial.

Never add:

- `default/db.tar.gz` or any other node/Gateway database.
- `default/contracts.yaml`; zk-deployer's `state.json` and Safe manifest are transient
  deployment internals, while the node-required addresses are already in `config.yaml`.
- Gateway or multi-chain configuration files.

## Verify

Run the bundled verifier after generation:

```bash
REPO_ROOT=$(git rev-parse --show-toplevel)
"$REPO_ROOT/.agents/skills/generate-zksync-local-state/scripts/verify-local-state.sh" \
  --server-root /path/to/zksync-os-server \
  --fixture-dir /path/to/zksync-os-server/local-chains/v32.0
```

The verifier loads the compact L1 snapshot, starts the node with a new temporary RocksDB, and
requires all of the following:

- No packaged DB or `contracts.yaml` exists.
- The node discovers the 10 default priority deposits.
- The protocol-upgrade block and deposit block execute.
- State-diff checks pass for both blocks.

It writes no verification DB into the repository. If verification fails, inspect the preserved
temporary directory and report the exact blocker.

## Review

Run `git diff --check`, inspect the complete version-directory diff, and confirm:

- The configured Bridgehub and bytecode-supplier addresses match the node's startup L1 state.
- `versions.yaml` records the resolved full era-contracts, zk-deployer, and server commits.
- The gzip is materially smaller than an interval-mined snapshot.
- Only the requested single-chain fixture changed.

Report the output path, compressed size, L1 transaction/block counts, chain ID, protocol
version, source revisions, and verification result.
