# Single Chain (v31.0, multiprover L1)

Single-chain configuration for protocol version v31.0 on an L1 whose verifier
is the multiprover set. It is the v31.0 chain with the same chain ID, the same
ecosystem addresses and the same wallets, so it accepts the v31.0 configuration
unchanged apart from the genesis path.

## Chains

| Config            | Chain ID | RPC Port |
|-------------------|----------|----------|
| `config.yaml`     | 506      | 3050     |

## Quick Start

```bash
# Use script to launch in-memory L1 and the node for one chain
./run_local.sh ./local-chains/v31.0-multiprover/default
```

## Verifier

The chain's verifier requires an Airbender proof and an aggregated ZiSK proof
together. Set `prover_input_generator.second_proof_system` and
`prover_input_generator.multi_proof_verifier` to run the real settlement path.
Fake proofs settle without either flag: the testnet wrapper keeps the
empty-proof and mock-proof escape hatches.

`multi_proof_verifier` is validated fail-closed at startup, because a batch
whose second proof never arrives is held at the commit gate and stalls block
production rather than settling half-proved. Running it locally therefore also
means turning off what `local-chains/local_dev.yaml` switches on for the fake
path. The server pins the guest, aggregator, recursive setup, host prover and L1
identity in its compiled ZiSK V1 release manifest:

```yaml
prover_api:
  fake_fri_provers: { enabled: false }
  fake_snark_provers: { enabled: false }
prover_input_generator:
  enable_input_generation: true
  zisk_shadow_execution: true   # the local arbiter for a commitment mismatch
```

Real provers must be reachable over the prover API for any batch to settle:
with both fake pools off there is no in-process fallback for either lane, so
this needs real Airbender FRI/SNARK provers as well as real ZiSK ones. See [the parent
README](../README.md) for the deployed addresses and how to regenerate the
state.

## Wallets

For complete list of keys and wallet addresses, check [wallets.yaml](./wallets.yaml).

## Contract Addresses

For contract addresses, please refer to `genesis` section of the [config.yaml](./config.yaml).

## Versions

For information about how this config was created, check [versions.yaml](../versions.yaml).
