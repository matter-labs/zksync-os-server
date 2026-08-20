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

Updated again for the zksync-os v0.4.0 release binary (see `../versions.yaml` for the
VK provenance), by direct state edits — no new L1 blocks, so historical states stay 1:1
with block records:

- The verifier version 8 `ZKsyncOSVerifierPlonk` replaced with one generated from the
  v0.4.0-binary VK (`0x9f7576b9…`); the old registration verified the superseded
  draft-binary VK.
- The L2 genesis (`../genesis.json`) now deploys the v0.4.0-layout `L2AssetTracker`
  with its base-token asset id, L1 chain id, registration and migration-number slots
  initialized — since zksync-os v0.4.0 every block's pre-tx loop calls the tracker and
  fails fatally on an uninitialized or old-layout deployment. Its `genesis_root` was
  recalculated (see the `recompute_genesis_root` utility test in
  `zksync_os_batch_verification`), and the batch-0 hash stored on the diamond and the
  CTM (`storedBatchZero`) updated to match.
- `ZKsyncOSDualVerifier` code swapped to the `ZKsyncOSTestnetVerifier` build (storage,
  including verifier registrations, preserved): the previous in-place replacement used
  the production build, whose `mockVerify` reverts `MockVerifierNotSupported` — fake
  (mock) proofs from `local_dev.yaml`'s fake provers could never settle.
- Block records truncated to block 128, the last block with a persisted historical
  state (earlier regenerations added block records without states, making watcher
  startup probes fail nondeterministically with `BlockOutOfRangeError`). The dropped
  blocks' effects live in the current account state.

Validated end-to-end on this snapshot: the node starts from genesis, seals blocks, and
batches 1-2 were committed, (fake-)proved and executed on L1.

Regenerate with `local-chains/v32.0/regenerate.sh` after bumping the contracts.

## Quick Start

```bash
./run_local.sh ./local-chains/v32.0/default
```

Wallets and operator keys are in [wallets.yaml](./wallets.yaml). Node-required
contract addresses are in the `genesis` section of [config.yaml](./config.yaml).
Source revisions are recorded in [versions.yaml](../versions.yaml).
