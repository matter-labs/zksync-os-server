# zksync-os-sequencer
New sequencer implementation for zksync-os with focus on high throughput, low latency and great DevEx.

## Run
### Local
`cargo run --release` (`release` makes a significant difference for performance)

Rich account `0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110` (`0x36615Cf349d7F6344891B1e7CA7C72883F5dc049`)

To enable local L1 - in other tab (see "L1 State" below for more info)
```
anvil --load-state zkos-l1-state.json --chain-id 9 --port 8545
```

See `node/sequencer/config.rs` for config options and defaults. Use env variables to override, e.g. for high TPS:
```
batcher_component_enabled=false sequencer_blocks_to_retain_in_memory=16 cargo run --release
```

### Docker
```
sudo docker build -t zksync_os_sequencer .
sudo docker run -d --name sequencer -p 3050:3050 -p 3124:3124 -p 3312:3312 -e batcher_maximum_in_flight_blocks=15  -v /mnt/localssd/db:/db   zksync_os_sequencer
```

### Exposed Ports
* `3050` - L2 JSON RPC
* `3124` - Prover API (e.g. `127.0.0.1/prover-jobs/status`) (only enabled if `prover_api_component_enabled` is set to `true`)
* `3312` - Prometheus (compatible with `grafana_dashboard.json`)


### Prerequisites
Only Rust + Cargo.
When running on a new VM:
```
# essentials
sudo apt-get install -y build-essential pkg-config cmake clang lldb lld libssl-dev apt-transport-https ca-certificates curl software-properties-common git
# rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# add rust to PATH
. "$HOME/.cargo/env"    
```

### Resetting the Chain
Erase the local DB and re-run anvil:
```
rm -rf db/node1/*
anvil --load-state zkos-l1-state.json --chain-id 9 --port 8545
```

## High-level design
### Principles
* Low, async persistence
  * to meet throughput and latency requirements, we avoid synchronous persistence at the critical path. Additionally, we aim at storing only the data that is strictly needed - minimizing the potential for state inconsistency
* Few, meaningful data containers / abstractions. Examples:
  * We reuse `BatchOutput` and `BatchContext` structs that are introduced in `zksync-os`  - avoiding further container structs
  * We introduce key concept of `ReplayRecord` that has all info to replay a block (along from the key value state access):
```rust
pub struct ReplayRecord {
    pub context: BatchContext,
    pub transactions: Vec<Transaction>,
}
```
* State - strong separation between:
  1. Actual state - data needed to execute VM: key-value storage and preimages map - see `state` crate
  2. Receipts repositories - data only needed in API - see `repositories/mod.rs`
  3. Data related to Proofs and L1 - not needed by sequencer / JSON RPC - only introduced downstream from `batcher` - see below.
     
Minimal Node will only have (1).
* Explicit finality handling - description WIP

### Components
Note: contrary to the Era sequencer, one Node = one process - regardless of the configuration
* **Sequencer** - mandatory component (every node runs it), executes transactions in VM, sends results downstream to other components.
  * Handles `Produce` and `Replay` commands in an uniform way (see `model/mod.rs` and `execution/block_executor.rs`)
  * For each block: (1) persists it in WAL (see `block_replay_storage.rs`), (2) pushes to `state` (see `state` batch), (3) exposes the block and tx receipts to API (see `repositories/mod.rs`), (4) pushes to async channels for `batcher` and `tree` components.
* **JSON RPC** - optional component (not configurable atm). Has shared access to `state`.
* **Persistent Tree** - optional component (not configurable atm), gets state updates via async channel, updates and persists the tree. Exposes root and merkle proofs for the prover input generation (note that sequencer itself doesn't need the tree - our block hash doesn't include root hash)
* **Batcher** - optional component (configurable with `batcher_component_enabled=true/false`).
  * Turns a stream of blocks to a stream of batches (1 batch = 1 proof = 1 L1 commit)
    * Note: currently 1 block == 1 batch, multi-block batches are not implemented yet 
  * For each batch, computes the Prover Input (runs RiscV binary (`server_app.bin`) and records its input as a stream of `Vec<u32>` - see `batcher/mod.rs`)
    * This process requires Merkle Tree with materialized root hashes and proofs at every block boundary (not batch!). This may be optimized later on. 
    * Process multiple batches in parallel (prover input generation takes more time than block execution)
  * Downstream:
    * Sends the resulting Prover Input to the **Prover API** (see below)
    * Sends the batch data (`BatchCommitData`) to the **L1 sender** (see below)
* **L1 sender** - optional component (runs if `localhost:8545` is available). Similar to the Era l1 sender
  * Doesn't need persistence (recovers its state from L1)
    * Currently still persists all sent batches - only to have the previous batch data that is needed when commiting. This will be changed.
* **Prover API** - opional component (configurable with `prover_api_component_enabled=true/false`). keeps unproved batches in memory (see `prover_api/prover_job_manager.rs`). Backpressures if there are more than `prover_api_max_unproved_blocks` of them. Exposes the following api (see `prover_api/prover_server.rs`):
```
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/available", get(list_available_proofs))
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
```
Note that it's the same api as in old sequencer integration, so FRI GPU Provers themselves are fully compatible. 
  * Stores submitted FRI proofs in RocksDB and exposes them via API (under `/prover-jobs/FRI/:block`)
<img width="1500" height="756" alt="Screenshot 2025-07-17 at 14 51 12" src="https://github.com/user-attachments/assets/cc8a27f0-15df-4406-b803-0e960832a4f1" />

## Known restrictions

* lacking seal criteria
* older blocks not available for eth_call
* no upgrade/interop logic

## Glossary (WIP)

* `Block` vs `Batch`:
  * One `block` = one vm run in block_executor = one block_receipt, 
  * one `batch` = one FRI proof = one L1 commit.

Important: currently zksync-os uses term `batch` for blocks (e.g. `run_batch` etc.). 
Also, return type of a block is `BatchOutput` - which represents a block in our case. 
todo: use `use BatchOutput as BlockOutput` in this repo to avoid confusion.

## L1 State

Please see the [How to run with L1 doc](docs/running_with_l1.md)