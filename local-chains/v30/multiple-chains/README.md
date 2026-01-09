# Multiple Chains (v30)

Configuration for running multiple ZKsync OS chains against a shared L1.

## Chains

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| `chain1.json` | 6565 | 3050 |
| `chain2.json` | 6566 | 3051 |
| `chain3.json` | 6567 | 3052 |

All chains run in **ephemeral mode** (no persistence).

## Quick Start

```bash
# Terminal 1: Start Anvil with shared L1 state
anvil --load-state ./local-chains/v30/multiple-chains/zkos-l1-state.json --port 8545

# Terminal 2: Chain 1
cargo run --release -- --config ./local-chains/v30/multiple-chains/chain1.json

# Terminal 3: Chain 2
cargo run --release -- --config ./local-chains/v30/multiple-chains/chain2.json

# Terminal 4: Chain 3
cargo run --release -- --config ./local-chains/v30/multiple-chains/chain3.json
```

## Contract Addresses

- **Bridgehub**: `0xfad38cb30077bbfd8e1077451ce5f890567d5484`
- **Bytecode Supplier**: `0xb692c6c62cf9753cb96b4564b60da0a1e43b4e26`

## Era contracts branch used:
Branch `zkos-v0.30.2`
