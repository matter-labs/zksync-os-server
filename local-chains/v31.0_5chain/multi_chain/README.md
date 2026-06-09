# Multiple Chains (v31.0 — 5-chain interop loadtest ecosystem)

Configuration for running multiple ZKsync OS chains against a shared L1.

## Chains

| Config            | Chain ID | RPC Port | DA mode  |
|-------------------|----------|----------|----------|
| `chain_6565.yaml` | 6565     | 3050     | rollup   |
| `chain_6566.yaml` | 6566     | 3051     | rollup   |
| `chain_6567.yaml` | 6567     | 3053     | rollup   |
| `chain_6568.yaml` | 6568     | 3054     | validium |
| `chain_6569.yaml` | 6569     | 3055     | validium |

The gateway chain `506` listens on port `3052` and the in-memory L1 (Anvil) on
`8545`.

## DA modes (rollup vs validium)

The committed `l1-state.json.gz` / `genesis.json` deploy a **mixed** ecosystem:
`6565`–`6567` are **rollup** chains and `6568`, `6569` are **validium**
(`no-da`) chains. This is **not** a runtime flag — the DA mode is baked into each
chain's on-chain deployment (its diamond proxy and L1 DA validator) captured in
the L1 state snapshot. To change which chains are validium you must regenerate
the snapshot (see below). An all-rollup snapshot is **not** committed.

## Quick Start

```bash
# Use script to launch in-memory L1, gateway, and five child chains (mixed DA)
./run_local.sh ./local-chains/v31.0_5chain/multi_chain
```

For driving interop load against this setup (20 TPS/chain, both with and without
validiums) and reading the latency results, see
[interop-load/README.md](../../../interop-load/README.md#running-the-5-chain-interop-load-test).

## Regenerating the L1 state

The `l1-state.json.gz`, `genesis.json`, and per-chain `contracts_*.yaml` /
`wallets_*.yaml` are produced by the upgrade scripts in
[`zksync-os-scripts`](https://github.com/matter-labs/zksync-os-scripts)
(`scripts/update_server.py`), pinned via `sha` in
[`../versions.yaml`](../versions.yaml).

Which chains are validium is set at the call site in `update_server.py`:

```python
user_chains = ["6565", "6566", "6567", "6568", "6569"]
setup = GatewaySetup(
    "multi_chain",
    user_chains,
    config.GATEWAY_CHAIN_ID,
    validium_user_chains=("6568", "6569"),   # ← these chains deploy as validium
)
```

- **Mixed (committed default):** `validium_user_chains=("6568", "6569")`.
- **All rollup (not committed):** set `validium_user_chains=()`, regenerate, and
  copy the resulting `l1-state.json.gz` / `genesis.json` / `contracts_*.yaml` /
  `wallets_*.yaml` into this directory (and `../l1-state.json.gz`,
  `../genesis.json`) before running. The chain `*.yaml`, gateway, and harness
  invocations are otherwise unchanged.

Validium backend is `no-da` (`validium_type_for` → `"no-da"`); the matching L1
DA validator is `no_da_validium_l1_validator_addr` in each `contracts_*.yaml`.

## Wallets

For complete list of keys and wallet addresses, check:
* [wallets_6565.yaml](./wallets_6565.yaml)
* [wallets_6566.yaml](./wallets_6566.yaml)
* [wallets_6567.yaml](./wallets_6567.yaml)
* [wallets_6568.yaml](./wallets_6568.yaml)
* [wallets_6569.yaml](./wallets_6569.yaml)
  for the corresponding chain.

## Contract Addresses

For contract addresses, please refer to `genesis` section of:
* [chain_6565.yaml](./chain_6565.yaml)
* [chain_6566.yaml](./chain_6566.yaml)
* [chain_6567.yaml](./chain_6567.yaml)
* [chain_6568.yaml](./chain_6568.yaml)
* [chain_6569.yaml](./chain_6569.yaml)
  for the corresponding chain.

## Versions

For information about how this config was created, check [version.yaml](../versions.yaml) file.
