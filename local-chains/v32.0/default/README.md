# Single Chain (v32.0)

Default single-chain configuration for running ZKsync OS directly against L1 for protocol version 0.32.0.

## Chain

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| `config.yaml` | 506 | 3050 |

The ecosystem and chain were deployed with `zk-deployer`. No Gateway chain,
Gateway database, or pre-generated node database is included. The L1 snapshot
contains 129 transaction blocks and no interval-mined empty blocks.

The snapshot has since been upgraded in place so the chain can verify V8
(proving version 8) proofs, which the original deployment could not:

- `ZKsyncOSVerifierPlonk` for the v32.0 VK deployed and registered on the chain's
  `ZKsyncOSDualVerifier` at **verifier version 8** — the version the server encodes in
  `_proof[0]` for V8 proofs. Version 0 still holds the V7 verifier.
- `ExecutorFacet` and `CommitterFacet` replaced via diamond cut with builds from
  era-contracts [`7644cc62`](https://github.com/matter-labs/era-contracts/pull/2381):
  era-contracts#2323 (chain config hash in the batch proof public input, chain-id-less
  `batchOutputHash`) plus the full-hash multi-batch fold.
- `ZKsyncOSDualVerifier` code replaced in place with the same build, preserving its
  verifier mappings.

Regenerate with `local-chains/v32.0/regenerate.sh` after bumping the contracts.

## Quick Start

```bash
./run_local.sh ./local-chains/v32.0/default
```

Wallets and operator keys are in [wallets.yaml](./wallets.yaml). Node-required
contract addresses are in the `genesis` section of [config.yaml](./config.yaml).
Source revisions are recorded in [versions.yaml](../versions.yaml).
