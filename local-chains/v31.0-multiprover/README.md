# Multiprover L1 (protocol v31.0)

The v31.0 chain on an L1 that verifies state transitions with the multiprover
set. `l1-state.json.gz` is the v31.0 state with four contracts deployed on top
and the chain's verifier pointer moved to the new wrapper. Every other v31.0
address, wallet and genesis file stays valid, so `genesis.json` and
`default/wallets.yaml` are symlinks into `../v31.0/` and `default/config.yaml`
differs from the v31.0 one in the genesis path only.

## Deployed set

| Contract | Address |
|---|---|
| `MultiProofTestnetVerifier` (the chain's verifier) | `0xb9bEECD1A582768711dE1EE7B0A1d582D9d72a6C` |
| `MultiProofVerifier` | `0xB82008565FdC7e44609fA118A4a681E92581e680` |
| `ZiskVerifier` | `0x5fc748f1FEb28d7b76fa1c6B07D8ba2d5535177c` |
| `ZiskSnarkPlonkVerifier` | `0x38a024C0b412B9d1db8BC398140D00F5Af3093D4` |
| Airbender PLONK verifier (from v31.0) | `0x40C2a18C576B4864b2cf3A458499137EE96057aA` |

`MultiProofTestnetVerifier` accepts an empty proof and a mock (type 3) proof,
so the fake provers settle here exactly as they do on v31.0. It delegates every
other proof to `MultiProofVerifier`, which accepts the combined type-5 payload
only and requires an Airbender proof and an aggregated ZiSK proof of the same
range. The Airbender half keeps the v31.0 PLONK verifier, so Airbender
verification is byte-for-byte the v31.0 check.

`ZiskVerifier` pins the ZiSK guest keys as compile-time constants:

| Pin | Value |
|---|---|
| `innerProgramVK` | `0x44e3d132399c8f3a03ce9672ba0ca00c6503db918731c7ab46d6faea445236ec` |
| `aggregatorProgramVK` | `0x4c3d7317a62f651d813ba6afbbce59e45eaa7c009ab2a9b51d2f0fb3e7987254` |
| `rootCVadcopFinal` | `0xcf2a309856f107b143836ada112806da71ae11567fa3f2d2050baba5381c7b7d` |
| `verificationKeyHash()` | `0x718bdb59530514f9a62f16b2ba912de17188615d82aa31ec681be4b9cd332888` |

A guest ELF rotation rotates these pins, which means a regeneration of
`ZiskVerifier` in era-contracts and a rebake here.

## Regenerating the state

`bake-l1-state.sh` loads `../v31.0/l1-state.json.gz` into anvil, deploys the
four contracts, points the diamond's `verifier` storage slot at the wrapper,
checks every address and pin, and dumps the state. It needs an era-contracts
checkout that carries the multiprover verifiers and the generated snarkJS
Plonk verifier:

```bash
ERA_CONTRACTS=/path/to/era-contracts ./bake-l1-state.sh
```

The deployer is anvil development account 0, which also owns the v31.0 verifier
registry, so the deployment is deterministic and the addresses above reproduce.

## Why a storage write

The diamond's verifier can otherwise only move through a protocol upgrade, and
an upgrade raises the chain's protocol version — which would take the chain off
v31.0. The script therefore writes the verifier slot directly and then asserts
`getVerifier()`. The rest of the state is untouched: the only differences from
v31.0 are the four new accounts, that one slot, and the deployer's nonce and
balance.
