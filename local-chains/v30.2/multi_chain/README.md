# Multiple Chains (v30.2)

Configuration for running multiple ZKsync OS chains against a shared L1.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `chain_6565.yaml` | 6565     | 3050     |
| `chain_6566.yaml` | 6566     | 3051     |

## Quick Start

```bash
# Use script to launch in-memory L1 and two nodes for both chains
./run_local.sh ./local-chains/v30.2/multi_chain
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

For information about how this config was created, check [versions.yaml](../versions.yaml) file.
