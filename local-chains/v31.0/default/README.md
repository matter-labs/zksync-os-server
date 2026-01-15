# Single Chain (v31.0)

Default single-chain configuration for running ZKsync OS against L1 for protocol version v31.0.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 6565     | 3050     |

## Quick Start

```bash
# Terminal 1: Start Anvil with shared L1 state
anvil --load-state ./local-chains/v31.0/default/zkos-l1-state.json --port 8545

# Terminal 2: Run the node
cargo run --release -- --config ./local-chains/v31.0/default/config.yaml
```

## Wallets

For complete list of keys and wallet addresses, check [wallets.yaml](./wallets.yaml).

## Contract Addresses

For contract addresses, please refer to `genesis` section of the [config.yaml](./config.yaml).

## Versions

For information about how this config was created, check [version.toml](../versions.toml) file.
