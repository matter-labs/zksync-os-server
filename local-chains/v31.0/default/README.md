# Single Chain (v31.0)

Default single-chain configuration for running ZKsync OS directly against L1 for protocol version v31.0.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 506      | 3050     |

The ecosystem and chain were deployed with `zk-deployer`; no Gateway chain or Gateway
database is involved. The node initializes its database from genesis on first start and
processes the priority transactions already present in the L1 state.

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

For information about how this config was created, check [versions.yaml](../versions.yaml).
