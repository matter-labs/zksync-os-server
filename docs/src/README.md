# ZKsync OS Server Developer Documentation

This book guides you through running, understanding, and extending the **ZKsync OS Server**.

## Understanding the System

Deep dives into internal components and lifecycle.

- [Database Layout](design/db.md)
- [State Model](design/state.md)
- [Merkle Tree Structure](design/tree.md)
- [Genesis Process](design/genesis.md)

## Running the System

These guides help you set up and operate the server in different environments.

- [Run against Layer 1 (L1)](guides/running_with_l1.md) — Local dev chain and Sepolia testnet setup, environment variables, common pitfalls.
- [Updating Contracts](guides/updating.md) — Rebuilding and deploying custom contracts, migration flow, testing.
