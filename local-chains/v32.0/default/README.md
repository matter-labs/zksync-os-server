# Single Chain (v32.0)

Default single-chain configuration for running ZKsync OS directly against L1 for protocol version 0.32.0.

## Chain

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| `config.yaml` | 506 | 3050 |

The ecosystem and chain were deployed with `zk-deployer` from the era-contracts
revision recorded in [versions.yaml](../versions.yaml): the atomic-interop
contracts converged with the V8 settlement layer (flat multi-batch public-input
fold, 100-bit V8 SNARK VK registered at verifier version 8). No Gateway chain,
Gateway database, or pre-generated node database is included. The L1 snapshot
contains 126 transactions and no interval-mined empty blocks.

## Quick Start

```bash
./run_local.sh ./local-chains/v32.0/default
```

Wallets and operator keys are in [wallets.yaml](./wallets.yaml). Node-required
contract addresses are in the `genesis` section of [config.yaml](./config.yaml).
Source revisions are recorded in [versions.yaml](../versions.yaml).
