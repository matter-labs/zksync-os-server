# Database Schema Overview

All persistent data is stored across multiple RocksDB databases:

- block_replay_wal
- preimages_full_diffs
- repository
- state_full_diffs
- tree
- proofs (JSON files, not RocksDB)

---

## 1. block_replay_wal

Write-ahead log containing recent block data.

| Column | Key | Value |
|--------|-----|--------|
| block_output_hash | block number | Block output hash |
| context | block number | Binary-encoded BlockContext (BlockMetadataFromOracle) |
| last_processed_l1_tx_id | block number | ID (u64) of the last processed L1 tx in the block |
| txs | block number | Vector of EIP-2718 encoded transactions |
| node_version | block number | Node version that produced the block |
| latest | 'latest_block' | Latest block number |

---

## 2. preimages_full_diffs

| Column | Key | Value |
|--------|-----|--------|
| storage | hash | Preimage for the hash |

---

## 3. repository

Canonical blocks and transactions.

| Column | Key | Value |
|--------|-----|--------|
| initiator_and_nonce_to_hash | address (20 bytes) + nonce (u64) | Transaction hash |
| tx_meta | transaction hash | Binary TxMeta (hash, number, gas used, etc.) |
| block_data | block hash | Alloy-serialized block |
| tx_receipt | transaction hash | Binary EIP-2718 receipt |
| meta | 'block_number' | Latest block number |
| tx | transaction hash | EIP-2718 encoded bytes |
| block_number_to_hash | block number | Block hash |

---

## 4. state_full_diffs

Every storage write, keyed by hashed key and block number: a read at block B is a single
reverse seek from `hashed_key || B`.

| Column | Key | Value |
|--------|-----|--------|
| data | hashed_key (32 bytes) \|\| block number (8 bytes, big-endian) | Storage value |
| meta | 'latest_block' | Latest block number |

---

## 5. tree

Merkle-like structure.

| Column | Key | Value |
|--------|-----|--------|
| default | composite (version + nibble + index) | Serialized Leaf or Internal node |
| key_indices | hash | Key index |

Note: The 'default' column also stores a serialized Manifest at key '0'.

---

## 6. proofs

Stored as JSON files in a separate directory:
../shared/fri_batch_envelopes
