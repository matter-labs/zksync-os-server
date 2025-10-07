# zk-os Server Documentation

This directory provides entry points for running and understanding the zk-os server. Start with Running the system to get a local or testnet instance working, then explore internals via Understanding the system.

## Running the system

These guides help you spin up the server in different environments and apply custom contract logic.

* [running_with_l1.md](running_with_l1.md) — Run against Layer 1 (local dev chain and Sepolia testnet): environment setup, config, typical pitfalls.
* [updating.md](updating.md) — Apply and test local contract changes: build pipeline, deployment flow, migration notes.

## Understanding the system

Deep dives into core components and lifecycle.

* [db.md](db.md) — Database layout: tables, key entities, indexing strategy.
* [genesis.md](genesis.md) — Startup (genesis) process: initialization order, invariant checks.
* [state.md](state.md) — State model: commitment formats, mutation flow, consistency rules.
* [tree.md](tree.md) — Merkle tree structure: node types, hashing scheme, update algorithm.

## Recommended Reading Order

1. running_with_l1.md
2. db.md
3. state.md
4. tree.md
5. genesis.md
6. updating.md (when modifying contracts)

## Need Help?

Check inline comments in the referenced files and project-level README for broader context.