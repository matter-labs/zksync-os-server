# Single Chain (v31.0)

Default single-chain configuration for running ZKsync OS against L1 for protocol version v31.0.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 506      | 3050     |

The chain starts from a pre-baked node database (`db.tar.gz`, unpacked into a temporary
directory via `general.ephemeral_state`) because the L1 state already contains batches
committed by the node run that produced these fixtures.

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
