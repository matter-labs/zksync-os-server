# Single Chain (v31.0)

Default single-chain configuration for running ZKsync OS against L1 for protocol version v31.0.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 6565     | 3050     |

## Quick Start

```bash
# Use script to launch in-memory L1 and the node for one chain
./run_local.sh ./local-chains/v31.0/default
```

## Wallets

For complete list of keys and wallet addresses, check [wallets.yaml](./wallets.yaml).

## Contract Addresses

For contract addresses, please refer to `genesis` section of the [config.yaml](./config.yaml).

## Versions

For information about how this config was created, check [version.yaml](../versions.yaml) file.
