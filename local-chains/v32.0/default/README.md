# Single Chain (v32.0)

Default single-chain configuration for running ZKsync OS directly against L1 for protocol version 0.32.0.

## Chain

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| `config.yaml` | 506 | 3050 |

The ecosystem and chain were deployed with `zk-deployer`. No Gateway chain,
Gateway database, or pre-generated node database is included. The L1 snapshot
contains 129 transaction blocks and no interval-mined empty blocks.

## Quick Start

```bash
./run_local.sh ./local-chains/v32.0/default
```

Wallets and operator keys are in [wallets.yaml](./wallets.yaml). Node-required
contract addresses are in the `genesis` section of [config.yaml](./config.yaml).
Source revisions are recorded in [versions.yaml](../versions.yaml).
