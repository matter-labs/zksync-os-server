# Consensus contracts

Production contracts owned by the consensus layer. Today that is one contract:
`ValidatorRegistry`, the on-chain home of consensus committee membership.

## Temporary home

This directory is a deliberately temporary arrangement. The registry was built
entirely within this repository so that the consensus integration could ship it
without a cross-team dependency; longer term it either moves to the protocol
contracts repository (with an official system address and real governance
wiring) or the teams decide that consensus-owned contracts stay local to this
repo. Either outcome is non-breaking by design: consensus nodes locate the
registry through configuration (`consensus.registry_address`) and speak to it
only through its versioned storage layout, so "moving" the contract is a config
change plus a file relocation, never a consensus behavior change.

## How it is consumed

Consensus nodes never call the contract. They read its storage slots directly
from their own finalized state, which makes the storage layout the interface:

- the layout is hand-assigned and documented in the contract source
  (`src/ValidatorRegistry.sol`);
- `lib/consensus/registry` mirrors it constant-for-constant on the node side;
- integration tests pin the two against each other slot-by-slot, and pin the
  compiled bytecode against the checked-in copy under
  `lib/consensus/registry/pinned/` (regenerating that pin is a deliberate,
  reviewed act, like regenerating wire goldens).

Layout evolution is versioned and additive: slot 0 holds the layout version,
readers dispatch on it, and a new layout is a new version with its own reader —
existing slots are never reinterpreted.

## Building

Artifacts are built by `integration-tests/build.rs` (which runs
`forge build --root ../contracts` alongside the test contracts), or manually:

```shell
forge build
```
