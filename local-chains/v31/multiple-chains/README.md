# Multiple Chains (v31)

Configuration for running multiple ZKsync OS chains against a shared L1.

## Chains

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| `chain1.json` | 6565 | 3050 |
| `chain2.json` | 6566 | 3051 |

All chains run in **ephemeral mode** (no persistence).

## Quick Start

```bash
# Terminal 1: Start Anvil with shared L1 state
anvil --load-state ./local-chains/v31/multiple-chains/zkos-l1-state.json --port 8545

# Terminal 2: Chain 1
cargo run --release -- --config ./local-chains/v31/multiple-chains/chain1.json

# Terminal 3: Chain 2
cargo run --release -- --config ./local-chains/v31/multiple-chains/chain2.json
```

## Contract Addresses

- **Bridgehub**: `0xb318b56e313d15e61467d894c431ada085a7a5ae`
- **Bytecode Supplier**: `0xd7313cfbc527956f36c13c1db8f7ce7ef91eb40b`

## Era contracts branch used:
Branch `zksync-os-with-kl-medium-interop`
