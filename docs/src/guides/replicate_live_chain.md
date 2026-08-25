# Replicate a live chain locally

Run a local `zksync-os-server` as a drop-in replica of a live chain (a testnet, `stage`, or any
deployed environment) by forking its L1 into [anvil](https://book.getfoundry.sh/anvil/) and
replaying the chain's recovered `block_replay_wal`. The node then commits, proves, and executes
batches against the fork using fake provers — no real prover or L1 funds required.

This is useful for reproducing a production issue against real chain state, testing an upgrade
against a real history, or debugging batch settlement locally.

```admonish info
The request chain is `server → anvil (:8545) → [optional eRPC cache] → your L1 RPC`. The server
talks only to anvil, never to L1 directly — it must see the mutated fork (impersonated operators,
its own commits), which the upstream does not have.
```

## Prerequisites

- [Foundry](https://book.getfoundry.sh/) (`anvil`, `cast`), `python3`, `curl`, `jq`.
- An authenticated **L1 RPC URL** for the chain's settlement layer, exported as `L1_RPC`. It must
  serve archive state at the fork block (`eth_getStorageAt`/`debug_traceCall` at historical blocks).
- Access to the chain's config (its `genesis.json` and server `common.yaml`) — e.g. via `kubectl`
  for a k8s deployment.
- Either a recovered `block_replay_wal` (a replay-archive recovery) for a fast start, or nothing —
  you can also start from genesis and let the node replay the whole history from L1.

Throughout, set your environment specifics once:

```bash
export L1_RPC="https://<your-authenticated-l1-rpc>"   # never commit this — it usually carries a token
export NS="<your-namespace>"                          # k8s namespace of the deployment, if applicable
```

## Step 1 — Fetch the chain's config

Configmap names may carry kustomize hash suffixes, so look them up rather than hardcoding:

```bash
kubectl -n "$NS" get configmap genesis-config -o jsonpath='{.data.genesis\.json}' > genesis.json
kubectl -n "$NS" get configmap \
  "$(kubectl -n "$NS" get configmap -o name | grep sequencer-config-common | cut -d/ -f2)" \
  -o jsonpath='{.data.common\.yaml}' > config.yaml
```

Note two values from `config.yaml` — you will reuse them:

- `genesis.chain_id` → `CHAIN_ID`
- `genesis.bridgehub_address` → `BRIDGEHUB`

Everything else on-chain (the chain's diamond proxy, the ValidatorTimelock) is derived from these.

## Step 2 — Pick the fork block

```admonish warning title="The one decision that must be right"
Fork L1 at the block matching the moment your replay WAL snapshot was taken — **never later**. The
live sequencer keeps committing; if L1 at the fork block knows about batches whose L2 blocks are
not in your WAL, the node cannot reconcile and startup fails. Earlier is always safe (the node just
re-commits more itself); later is not.
```

```bash
# assumes ~12s L1 blocks (Ethereum/Sepolia); adjust the divisor for other settlement layers.
# macOS `date` shown; on GNU/Linux use: date -d "<snapshot date> <time>" +%s
SNAPSHOT_TS=$(date -j -f "%Y-%m-%d %H:%M" "<snapshot date> <time>" +%s)
CUR=$(cast block-number --rpc-url "$L1_RPC")
CURTS=$(cast block "$CUR" --rpc-url "$L1_RPC" --field timestamp)
FORK_BLOCK=$(( CUR - (CURTS - SNAPSHOT_TS) / 12 - 50 ))   # -50 ≈ 10 min safety margin
```

Starting from scratch (no WAL) instead? Any recent finalized block works — the node discovers batch
state from L1 and replays from genesis, but startup then replays every block the chain ever produced.

## Step 3 — (Optional) cache the fork with eRPC

A fresh start issues thousands of `eth_getLogs` windows and historical state reads against L1. An
[eRPC](https://docs.erpc.cloud/) cache in front of your L1 RPC makes repeat starts dramatically
faster (finalized responses are cached forever). Point anvil's `--fork-url` at the eRPC endpoint
instead of `$L1_RPC`. This is optional; without it, anvil forks `$L1_RPC` directly.

## Step 4 — Start anvil and authorize the operators

```bash
anvil --port 8545 --fork-url "$L1_RPC" \
  --fork-block-number "$FORK_BLOCK" --block-time 0.25 --mixed-mining --slots-in-an-epoch 10

# in another shell: impersonate the commit/prove/execute operators on the fork
CHAIN_ID="$CHAIN_ID" BRIDGEHUB="$BRIDGEHUB" ./local-chains/replica/setup-anvil.sh
```

[`setup-anvil.sh`](https://github.com/matter-labs/zksync-os-server/blob/main/local-chains/replica/setup-anvil.sh)
syncs the fork timestamp, funds the three anvil dev accounts, and authorizes them as operators.

```admonish info title="Why authorization has two layers"
The post-V29 `ValidatorTimelock` gates operators in two independent ways, and **both** are required:

- `isValidator(chain, account)` — gates **commit**.
- `hasRole(chain, role, account)` — per-chain `COMMITTER`/`PROVER`/`EXECUTOR_ROLE`, gates **prove**
  and **execute**.

Set only `isValidator` and the node commits fine, then panics with `RoleAccessDenied` on its first
prove. The script grants both by deriving each storage slot at runtime (it traces the contract's
own `SLOAD`), so it needs no hardcoded slots and works across redeploys and chains.
```

## Step 5 — Point the DB path at the replay WAL

The server opens `<rocks_db_path>/block_replay_wal`. A recovered WAL comes in one of two shapes:

- The directory **contains** `block_replay_wal/` → use it directly as `rocks_db_path`.
- The directory **is** the WAL (RocksDB `*.sst` files at top level) → nest it first:
  `mkdir -p db_root && mv <recovered> db_root/block_replay_wal`, then use `db_root`.

The state tree, repositories, and batch storage are rebuilt from the WAL on first start. Delete any
stale `fri_proofs/` left over from a previous snapshot.

## Step 6 — Start the server

```bash
export DB_ROOT=<parent of block_replay_wal>
export genesis_genesis_input_path=<path to genesis.json>
export general_l1_rpc_url="http://localhost:8545"          # anvil, NOT the eRPC/L1 endpoint
export general_rocks_db_path="${DB_ROOT}"
export rpc_address="0.0.0.0:3051"                           # default is 3050; override to avoid clashes
export network_enabled="false"
export batch_verification_server_enabled="false"
export batch_verification_client_enabled="false"
export l1_sender_fusaka_upgrade_timestamp="18446744073709551615"   # keep pre-Fusaka blob format
export prover_api_fake_fri_provers_enabled="true"
export prover_api_fake_fri_provers_compute_time="200ms"
export prover_api_fake_fri_provers_min_age="0ms"
export prover_api_fake_snark_provers_enabled="true"
export prover_api_fake_snark_provers_max_batch_age="0ms"
export prover_api_proof_storage_path="${DB_ROOT}/fri_proofs"
# Skip prover-input generation: on a WAL recovery it would compute a witness for every historical
# block (all skipped by the batcher) and stall the pipeline. Fake provers work without real inputs.
export prover_input_generator_enable_input_generation="false"
# Operators = anvil dev accounts 0/1/2 (commit/prove/execute). Anvil prints their private keys at
# startup — paste them here. A plain hex key overrides any gcp_kms signer in the fetched config,
# so no cloud auth is needed locally.
export l1_sender_operator_commit_sk="<anvil account #0 private key>"
export l1_sender_operator_prove_sk="<anvil account #1 private key>"
export l1_sender_operator_execute_sk="<anvil account #2 private key>"

./zksync-os-server --config config.yaml
```

## What a healthy startup looks like

These phases are one-time and log-heavy — they are **not** hangs:

- **L1 event scans** (`priority_tx` / `persist_batch` "received new events"): the node rebuilds its
  L1 view in 1000-block windows from the chain's deployment up to the fork block.
- **WAL replay**: every block is replayed before the node goes live — minutes for a large history.
- **Priority-tree rebuild (~10+ min, silent)**: on startup the priority tree logs
  `adding missing blocks to priority tree` and walks every executed block from scratch, holding its
  lock the whole time. During this window batches **commit and prove but do not execute**, so no
  `▶▶▶ Batch has been fully processed` lines appear yet. This looks like a stall but is not — the
  `re-built priority tree` line marks completion, after which execute and "fully processed" resume.

Once caught up, query the node on your `rpc_address` port (e.g. `cast block-number --rpc-url
http://localhost:3051`) and send transactions to it as usual.

## Troubleshooting

```admonish bug title="The L2 head stops advancing while CPU is idle"
A pipeline component fell behind and the backpressure monitor gated the source. It names the culprit
at `WARN` (`pipeline backpressure: ... causes: [...]`). Most common on a local replica:
`prover_input_generator` — set `prover_input_generator_enable_input_generation=false` (Step 6).
```

```admonish bug title="Server crashes at startup with BlockOutOfRangeError"
The startup batch-state search reads historical L1 state, but anvil only retains state for its most
recent few thousand self-mined blocks. Once anvil has mined past that window (tens of minutes), a
server restart can no longer read the fork-era state. Restart anvil fresh at the same fork block
(re-run `setup-anvil.sh`) before restarting the server. Pause mining while the server is down with
`cast rpc evm_setIntervalMining 0` (resume with `... 1`, integer seconds only).
```

```admonish bug title="Node can't find L2 blocks for a batch L1 says is committed"
Your fork block is later than the WAL snapshot (see Step 2). Re-fork a few hundred blocks earlier.
```
