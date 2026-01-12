# Protocol Version v30

Configuration files for running ZKsync OS with protocol version v30.

## Files

| File | Description |
|------|-------------|
| `config.json` | Node configuration (chain ID: 6565) |
| `genesis.json` | Genesis configuration |
| `zkos-l1-state.json` | L1 state snapshot for Anvil |

## Quick Start

```bash
# Start Anvil
anvil --load-state ./local-chains/v30/zkos-l1-state.json --port 8545

# Start the node
cargo run --release -- --config ./local-chains/v30/config.json
```

## Contract Addresses

- **Bridgehub**: `0x8aeec25f84d73c079b318881e445404a3b0cd2d2`
- **Bytecode Supplier**: `0xabf0447fd9281124968e259e9b9062041e0c98fc`

## Era contracts branch used:
Branch `zkos-v0.30.2`
