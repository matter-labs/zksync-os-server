# Single Chain (v30.2)

Default single-chain configuration for running ZKsync OS against L1 for protocol version v30.2.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 6565     | 3050     |

## Quick Start

```bash
# Use script to launch in-memory L1 and the node for one chain
./run_local.sh ./local-chains/v30.2/default
```

## Wallets

For complete list of keys and wallet addresses, check [wallets.yaml](./wallets.yaml).

## Contract Addresses

For contract addresses, refer to `genesis` section of the [config.yaml](./config.yaml).

## Versions

For information about how this config was created, check [versions.yaml](../versions.yaml) file.
