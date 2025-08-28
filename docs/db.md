# DB Schema

All the data is stored in multiple RocksDB databases.

* block_replay_wal
* preimages
* proofs
* repository
* state
* tree


## Block replay wal

Write-ahead-log


| Column name | Key | Value |
|-------------|-----|-------|
| block_output_hash | block number | block output hash |
| context | block number | Bin-encoded BlockContext (a.k.a BlockMetadataFromOracle)  |
| last_processed_l1_tx_id (starting l1 serial id)| block number | id of the last processed L1 transaction in this block (u64) |
| txs | block number | Vector of 2718 encoded transactions |
| node_version | block number | node version used to create it |
| latest | 'latest_block' (single key) | number of the latest block |


## Preimages

| Column name | Key | Value |
|-------------|-----|-------|
| meta | 'block' (only one key) | lastest block id |
| storage | hash | preimage matching this hash |

## Proofs 

| Column name | Key | Value |
|-------------|-----|-------|
| proofs | block number | proof (in json format) |

## Repository


| Column name | Key | Value |
|-------------|-----|-------|
| initiator_and_nonce_to_hash | concat of address (20 bytes) and nonce (as u64) | transaction hash (that this address created) |
| tx_meta | transaction_hash | bin serialized 'TxMeta' (hash, number, gas used etc) |
| block_data | block hash | Alloy-serialized block |
| tx_receipt | transaction hash | bin serialized 2718 receipt  |
| meta | 'block_number' (single key) | latest block|
| tx | transaction hash | 2718 encoded bytes |
| block_number_to_hash | block number | block hash |

## State

Keeps things that were 'compacted' from write-ahead log.

| Column name | Key | Value |
|-------------|-----|-------|
| meta | 'base_block' (single key) | block number |
| storage |  key | value (for compacted storage) |


## Tree

| Column name | Key | Value |
|-------------|-----|-------|
| default | key is concat of something (version + nibble + index) | value? |
| key_indices |  some hash | key index |

'magic' manifest is stored in key 0.