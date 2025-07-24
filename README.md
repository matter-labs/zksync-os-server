# zksync-os-sequencer

New sequencer implementation for zksync-os with focus on high throughput, low latency and great DevEx.

## Run

### Local

`cargo run --release` (`release` makes a significant difference for performance)

Rich account `0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110` (
`0x36615Cf349d7F6344891B1e7CA7C72883F5dc049`)

To enable local L1 - in other tab (see "L1 State" below for more info)

```
anvil --load-state zkos-l1-state.json --chain-id 9 --port 8545
```

See `node/sequencer/config.rs` for config options and defaults. Use env variables to override, e.g.:

```
batcher_component_enabled=false sequencer_blocks_to_retain_in_memory=256 cargo run --release
```

### Docker

```
sudo docker build -t zksync_os_sequencer .
sudo docker run -d --name sequencer -p 3050:3050 -p 3124:3124 -p 3312:3312 -e batcher_maximum_in_flight_blocks=15  -v /mnt/localssd/db:/db   zksync_os_sequencer
```

### Exposed Ports

* `3050` - L2 JSON RPC
* `3124` - Prover API (e.g. `127.0.0.1/prover-jobs/status`) (only enabled if `prover_api_component_enabled` is set to
  `true`)
* `3312` - Prometheus

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

Note: contrary to the Era sequencer, one Node = one process — regardless of the configuration

### Subsystems

* **Sequencer** subsystem - mandatory component (every node runs it), executes transactions in VM, sends results
  downstream to other components.
    * Handles `Produce` and `Replay` commands in an uniform way (see `model/mod.rs` and `execution/block_executor.rs`)
    * For each block: (1) persists it in WAL (see `block_replay_storage.rs`), (2) pushes to `state` (see `state`
      batch), (3) exposes the block and tx receipts to API (see `repositories/mod.rs`), (4) pushes to async channels for
      `batcher` and `tree` components.
* **API** subsystem - optional component (not configurable atm). Has shared access to `state`.
* **Batcher** subsystem - optional (configurable via `batcher_component_enabled=true/false`).
    * Turns a stream of blocks into a stream of batches (1 batch = 1 proof = 1 L1 commit); exposes Prover APIs; submits
      batches and proofs to L1.
      Note: currently 1 block == 1 batch, multi-block batches are not implemented yet.
    * For each batch, computes the Prover Input (runs RiscV binary (`server_app.bin`) and records its input as a stream
      of `Vec<u32>` - see `batcher/mod.rs`)
    * This process requires Merkle Tree with materialized root hashes and proofs at every block boundary (not batch!).
      This may be optimized later on.
    * **L1 sender** - optional component (runs if `localhost:8545` is available). Similar to the Era l1 sender
        * Doesn't need persistence (recovers its state from L1)- _сurrently still persists all sent batches - only to
          have the previous batch data that is needed when commiting. This will be changed._

Note on **Persistent Tree** - it is only needed for Batcher Subsystem. Sequencer doesn't need the tree - block hashes
don't include root hash. But even when batcher subsystem is not enabled, we want to run the tree for potential failover

### Prover API

```
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/available", get(list_available_proofs))
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
```

Note that it's the same api as in old sequencer integration, so FRI GPU Provers themselves are fully compatible.

### Components

Note: parts of this table is WIP:

* `L1FinalityMonitor` is missing
* `FRI Prover API` and `SNARK Prover API` are currently same thing (`ProverApi`)
* There is no `SNARK Job Manager` - and thus we don't support multiple SNARK wrappers
* There is no `L1 Executor`

<img width="1500" height="756" alt="Screenshot 2025-07-17 at 14 51 12" src="https://github.com/user-attachments/assets/cc8a27f0-15df-4406-b803-0e960832a4f1" />

**State Recovery**:

1. Determine the Block number for sequencer to start with:
    1. Get the last canonized block number in WAL/Consensus - this is the
    2. Determine the last Batch number

Universal rules:

* Components

| Component                   | In-memory state                                                                | Persistence                                                                                                          | State Recovery                                                                                                                                         | 
|-----------------------------|--------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------| 
| **Command Source**          | -                                                                              | -                                                                                                                    | Needs the block number to launch the stream with                                                                                                       | 
| **BlockContextProvider**    | `next_l1_priority_id`; `block_hashes_for_next_block` (last 256 block hashes)   | -                                                                                                                    | `next_l1_priority_id`: take from the last `ReplayRecord`; `block_hashes_for_next_block`: take from 256 last `ReplayRecord`s                            |
| **L1Watcher**               | `Arc<DashMap<PriorityId, L1Envelope>>`                                         | - (WIP)                                                                                                              | WIP                                                                                                                                                    |
| **L2Mempool**               | _RETH Crate_ - prepared list of pending transactions                           | -                                                                                                                    | Not needed - built from JSON RPC requests                                                                                                              |
| **BlockExecutor**           | None 🔥                                                                        | -                                                                                                                    | -                                                                                                                                                      |
| **State**                   | All Storage Logs and Preimages for `blocks_to_retain_in_memory` last blocks    | Compacted state at some older block (`highest_block - blocks_to_retain_in_memory`): full state map and all preimages | Not needed - chains starts from block `highest_block - blocks_to_retain_in_memory`, so it recovers under normal operations                             |
| **Merkle Tree**             | None - persistence only                                                        | Full Merkle tree - including previous values on each leaf                                                            | Not needed                                                                                                                                             |
| _⬇️Batcher Components_      | _Components below operate on Batches - not Blocks_                             | _Components below must not rely on persistence - otherwise failover is not possible_                                 |                                                                                                                                                        |                              
| **Batcher**                 | Previous/Trailing Batch `CommitBatchInfo`                                      | -                                                                                                                    | Currently: need to start from block zero. Later - two options: (1) Load last committed `CommitBatchInfo` from L1 OR (2) reprocess last committed batch |                             
| **Prover Input Generator**  | None                                                                           | -                                                                                                                    | Not needed                                                                                                                                             |                             
| **FRI Job Manager**         | Gapless List of unproved batches with `ProverInput` and prover assignment info | -                                                                                                                    | Not needed - batches that still need FRI proofs will go through the pipeline again (see above)                                                         |                             
| **L1 Committer**            | None/Minimal                                                                   | - (WIP)                                                                                                              | Not needed - recovers itself from L1                                                                                                                   |                             
| **SNARK Job Manager** (WIP) | Gapless list of batches with their FRI proofs and prover assignment info       | -                                                                                                                    | Load blocks that are committed but not proved on L1 yet. Load their FRI proofs from FRI cache (WIP)                                                    |                             
| **L1 Executor** (WIP)       | None/Minimal (TODO: tree for `PriorityOpsBatchInfo`)                           | -                                                                                                                    | Not needed - recovers itself from L1                                                                                                                   |                             

### Principles

* Minimal, async persistence
    * to meet throughput and latency requirements, we avoid synchronous persistence at the critical path. Additionally,
      we aim at storing only the data that is strictly needed - minimizing the potential for state inconsistency
* Few, meaningful data containers / abstractions. Examples:
    * We reuse `BatchOutput` and `BatchContext` structs that are introduced in `zksync-os`  - avoiding further container
      structs
    * We introduce key concept of `ReplayRecord` that has all info to replay a block (along from the key value state
      access):

```rust
pub struct ReplayRecord {
    pub context: BatchContext,
    pub transactions: Vec<Transaction>,
}
```

* State - strong separation between:
    1. Actual state - data needed to execute VM: key-value storage and preimages map - see `state` crate
    2. Receipts repositories - data only needed in API - see `repositories/mod.rs`
    3. Data related to Proofs and L1 - not needed by sequencer / JSON RPC - only introduced downstream from `batcher` -
       see below.

Minimal Node only needs (1).

## Known restrictions

* lacking seal criteria
* older blocks not available for eth_call
* no upgrade/interop logic

## Glossary

* `Block` vs `Batch`:
    * One `block` = one vm run in block_executor = one block_receipt,
    * one `batch` = one FRI proof = one L1 commit.

Important: currently zksync-os uses term `batch` for blocks (e.g. `run_batch` etc.).
Also, return type of a block is `BatchOutput` - which represents a block in our case.
todo: use `use BatchOutput as BlockOutput` in this repo to avoid confusion.

## L1 State

Please see the [How to run with L1 doc](docs/running_with_l1.md)