# verify-storage-proof

Verifies ZKsync storage slot values against L1 batch commitments using `zks_getProof`.

## Build

```bash
cargo build -p zksync_os_verify_storage_proof --release
```

## Usage

With bridgehub auto-discovery (recommended):

```bash
cargo run -p zksync_os_verify_storage_proof -- \
  --l2-rpc https://mainnet.era.zksync.io \
  --l1-rpc https://eth.llamarpc.com \
  --bridgehub 0x303a465B659cBB0ab36eE643eA362c509EEb5213 \
  --address 0x... \
  --keys 0x...,0x... \
  --batch-number 12345
```

With explicit diamond proxy address:

```bash
cargo run -p zksync_os_verify_storage_proof -- \
  --l2-rpc http://localhost:3050 \
  --l1-rpc http://localhost:8545 \
  --l1-contract 0x... \
  --address 0x... \
  --keys 0x... \
  --batch-number 1
```

## How it works

1. Fetches a Merkle storage proof from L2 via `zks_getProof(address, keys, batchNumber)`
2. Verifies the proof internally (Blake2s Merkle tree, depth 64) and computes the state commitment
3. Fetches the `batchHash` from the L1 `BlockCommit` event for the given batch and confirms it matches the computed state commitment

If auto-discovery is used (`--bridgehub`), the tool calls `eth_chainId` on L2 and `bridgehub.getZKChain(chainId)` on L1 to find the diamond proxy address.

## Options

| Flag | Required | Description |
|------|----------|-------------|
| `--l2-rpc` | Yes | L2 JSON-RPC endpoint |
| `--l1-rpc` | Yes | L1 JSON-RPC endpoint |
| `--address` | Yes | Account address to prove storage for |
| `--keys` | Yes | Comma-separated storage keys to verify |
| `--batch-number` | Yes | L1 batch number to verify against |
| `--bridgehub` | * | Bridgehub address on L1 (enables auto-discovery) |
| `--l1-contract` | * | Diamond proxy address on L1 (skips auto-discovery) |

\* One of `--bridgehub` or `--l1-contract` must be provided.

## Integration tests

The integration tests live in `integration-tests/tests/storage_proof.rs` and exercise the library against a local node with L1 (Anvil). Each test manages its own L1/node instance — no external setup required.

```bash
cargo nextest run -p zksync_os_integration_tests --test storage_proof
```
