# Protocol Version v31

Configuration files for running ZKsync OS with protocol version v31.

## Files

| File | Description |
|------|-------------|
| `config.json` | Node configuration (chain ID: 6565) |
| `genesis.json` | Genesis configuration |
| `zkos-l1-state.json` | L1 state snapshot for Anvil |

## Quick Start

```bash
# Start Anvil
anvil --load-state ./local-chains/v31/zkos-l1-state.json --port 8545

# Start the node
cargo run --release -- --config ./local-chains/v31/config.json
```

## Contract Addresses

- **Bridgehub**: `0xb318b56e313d15e61467d894c431ada085a7a5ae`
- **Bytecode Supplier**: `0xd7313cfbc527956f36c13c1db8f7ce7ef91eb40b`

## Era contracts branch used:
Branch `zksync-os-with-kl-medium-interop`
