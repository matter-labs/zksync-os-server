---
name: generate-zksync-local-state
description: Generate and verify DB-free, single-chain ZKsync OS local-chain fixtures with zk-deployer for arbitrary protocol versions and era-contracts Git revisions. Use when creating or regenerating local-chains/v31, v32, or later fixture directories; changing the contracts revision or chain ID; producing compact Anvil l1-state.json.gz snapshots; or replacing Gateway/multi-chain fixtures with a direct-L1 single-chain setup.
---

# Generate ZKsync Local State

Generate a self-consistent local fixture from a selected era-contracts commit. Keep the L1
snapshot compact by mining only submitted transactions during deployment. Do not package a
node database or `contracts.yaml`.

All process lifecycle — starting Anvil, waiting for it, flushing its `--dump-state` dump, and
guaranteeing teardown of Anvil and every process a block spawned — is owned by the bundled
`scripts/anvil-session.sh` harness. Everything else in this skill you perform directly with
your own tools so it tracks zk-deployer as its schema and commands evolve. Do **not** freeze
deployer specifics into a committed script.

## Gather inputs

Determine:

- ZKsync OS server repository root.
- era-contracts commit hash. Require the user to provide or confirm this value.
- Protocol version such as `v31`, `v31.0`, or `v32`. The fixture directory and the
  `protocol_version` recorded in `versions.yaml` are both `v<minor>.<patch>` (patch defaults
  to `0`).
- L2 chain ID; default to `506` only when the user does not specify one.
- zk-deployer repository and revision. Default to the sibling
  `zksync-os-integration-tests` checkout at `HEAD`; select a compatible revision if the chosen
  contracts commit does not compile with it.

Read the current zk-deployer README before generation because its intent schema and commands
may evolve:

`<zk-deployer-repo>/bin/zk-deployer/README.md`

Inspect repository instructions and existing worktree changes before writing. Preserve
unrelated changes.

## Build zk-deployer against the requested contracts

1. Create a detached temporary worktree of zk-deployer at the resolved revision (use
   `git worktree add --detach`). Build there so the main checkout is untouched.
2. In the worktree `Cargo.toml`, pin **both** matter-labs/era-contracts dependencies
   (`protocol_ops` and `zksync_os_genesis_gen`) to `rev = "<contracts-commit>"` with `Edit`.
   Read the file first and confirm both lines changed — a silent miss produces a fixture for
   the wrong contracts.
3. `cargo update -p protocol_ops -p zksync-os-genesis-gen`, then
   `cargo build --release -p zk-deployer --bin zk-deployer` (point `--target-dir` at the main
   repo `target/` to reuse its cache).
4. Resolve the full 40-char era-contracts SHA for `versions.yaml`:
   `cargo metadata --format-version 1 | jq -r '.packages[] | select(.name=="protocol_ops") | .source | capture("#(?<sha>[0-9a-f]{40})$").sha'`.

## Deploy against a throwaway L1

Create a scratch deployment directory, then with `Write` create both files from the **current**
README — do not assume the fields or command names from a previous run:

- `intent.yaml` — the current intent schema for one direct-L1 rollup chain at the chosen chain
  ID, pointing `l1_rpc_url` at `http://127.0.0.1:8545`.
- `deploy-block.sh` — the current deployer command sequence, e.g.:

  ```bash
  set -Eeuo pipefail
  cd "$WORKDIR"
  "$ZK_DEPLOYER" build-contracts
  "$ZK_DEPLOYER" bootstrap --broadcast
  "$ZK_DEPLOYER" apply --broadcast
  "$ZK_DEPLOYER" server-config --chain "$CHAIN_ID" --output server.yaml
  ```

Run the deployment through the harness. Anvil mines only submitted transactions (no block
interval), so every saved block holds a deployment or funding transaction. `$WORKDIR` and
`$L1_RPC` are exported into the block; export `$ZK_DEPLOYER` and `$CHAIN_ID` yourself:

```bash
ZK_DEPLOYER=/path/to/zk-deployer CHAIN_ID=506 \
  scripts/anvil-session.sh --workdir "$WORKDIR" --port 8545 \
    --preserve-historical-states --slots-in-an-epoch 2 \
    --dump-state "$WORKDIR/l1-state.json" \
    -- bash "$WORKDIR/deploy-block.sh"
```

The harness SIGINTs Anvil on exit so the dump flushes, and reaps the deployer if anything
fails. It never overwrites a fixture directory — that guard lives in the write step below.

## Assert the snapshot is transaction-only

Read `l1-state.json` and confirm with `jq -e` that there are no idle/interval-mined blocks:

```bash
jq -e '
  (.best_block_number == (.transactions | length))
  and ((.historical_states | length) == (.transactions | length))
  and ((.blocks | length) == ((.transactions | length) + 1))
' "$WORKDIR/l1-state.json"
```

If this fails, interval mining leaked in — stop and report; do not ship a bloated snapshot.

## Write the fixture

Stage into a scratch dir, then move into place. Target defaults to
`<server-root>/local-chains/v<minor>.<patch>`; refuse to overwrite an existing version
directory without explicit user authorization, and never write to `/` or the repo root.

- `l1-state.json.gz` — `gzip -9` of the snapshot.
- `genesis.json` — copied from the deployment.
- `default/wallets.yaml` — copied from the deployment.
- `default/config.yaml` — from the deployer `server.yaml`, with `genesis_input_path` rewritten
  to `./local-chains/v<minor>.<patch>/genesis.json`.
- `versions.yaml` — `Write` the resolved era-contracts, zk-deployer, and server SHAs plus a
  `general` block with `protocol_version: "v<minor>.<patch>"` and `verification_key: "TBD"`,
  matching the existing `local-chains/v*/versions.yaml` layout.
- `default/README.md` — quick-start pointing at `run_local.sh`, noting the transaction/block
  counts and that no Gateway or node DB is bundled.

Never add:

- `default/db.tar.gz` or any other node/Gateway database.
- `default/contracts.yaml`; zk-deployer's `state.json` and Safe manifest are transient
  deployment internals, while the node-required addresses are already in `config.yaml`.
- Gateway or multi-chain configuration files.

## Verify

Confirm the fixture boots with no packaged DB. First reject a fixture that ships one:

```bash
[[ ! -e "$FIXTURE_DIR/default/db.tar.gz" && ! -e "$FIXTURE_DIR/default/contracts.yaml" ]]
```

Decompress the snapshot, then `Write` a `verify.yaml` overlay in a scratch dir (temporary
`rocks_db_path`, `genesis_input_path` at the fixture's `genesis.json`, `enable_input_generation:
false`, `prover_api.enabled: false`, and unique RPC/status/prover/metrics ports). Build the
server if `target/release/zksync-os-server` is missing.

Author a `verify-block.sh` that starts the node against the loaded L1 and polls its RPC until
`eth_blockNumber >= 2`, failing fast if the process exits, then run it under the harness:

```bash
scripts/anvil-session.sh --workdir "$WORKDIR" --port 18545 \
  --load-state "$WORKDIR/l1-state.json" --block-time 0.25 --mixed-mining \
  --slots-in-an-epoch 10 \
  -- bash "$WORKDIR/verify-block.sh"
```

The node is launched with the layered config:
`--config local-chains/local_dev.yaml --config <fixture>/default/config.yaml --config <verify.yaml>`
and `L1_PROVIDER_RPC_URL` pointed at `$L1_RPC`. The harness reaps both the node and Anvil on
any exit, so no verification DB is written into the repository.

Then require, from the node log:

- The node discovers the 10 default priority deposits.
- The protocol-upgrade block and deposit block execute (L2 block reached ≥ 2).
- State-diff checks pass for both blocks.

If verification fails, inspect the preserved scratch directory and report the exact blocker.

## Review

Run `git diff --check`, inspect the complete version-directory diff, and confirm:

- The configured Bridgehub and bytecode-supplier addresses match the node's startup L1 state.
- `versions.yaml` records the resolved full era-contracts, zk-deployer, and server commits.
- The gzip is materially smaller than an interval-mined snapshot.
- Only the requested single-chain fixture changed.

Report the output path, compressed size, L1 transaction/block counts, chain ID, protocol
version, source revisions, and verification result.
