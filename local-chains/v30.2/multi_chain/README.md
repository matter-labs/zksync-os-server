# Multiple Chains (v30.2)

Configuration for running multiple ZKsync OS chains against a shared L1.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `chain_6565.yaml` | 6565     | 3050     |
| `chain_6566.yaml` | 6566     | 3051     |

## Quick Start

```bash
# Terminal 1: Start Anvil with shared L1 state
anvil --load-state ./local-chains/v30.2/multi_chain/zkos-l1-state.json --port 8545

# Terminal 2: Chain 1
cargo run --release -- --config ./local-chains/v30.2/multi_chain/chain_6565.yaml

# Terminal 3: Chain 2
cargo run --release -- --config ./local-chains/v30.2/multi_chain/chain_6566.yaml
```

## Wallets

For complete list of keys and wallet addresses, check:
* [wallets_6565.yaml](./wallets_6565.yaml)
* [wallets_6566.yaml](./wallets_6566.yaml)
for the corresponding chain.

## Contract Addresses

For contract addresses, please refer to `genesis` section of:
* [chain_6565.yaml](./chain_6565.yaml)
* [chain_6566.yaml](./chain_6566.yaml)
for the corresponding chain.

## Versions

For information about how this config was created, check [version.toml](../versions.toml) file.
