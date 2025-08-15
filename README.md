# zksync-os-sequencer

New sequencer implementation for zksync-os with focus on high throughput, low latency and great DevEx.

## Run

### Local

To run node locally, first launch `anvil`:
```
anvil --load-state zkos-l1-state.json --port 8545
```
then launch the server:
```
cargo run
```
Note that by default, fake (dummy) proofs are used both for FRI and SNARK proofs.

Rich account `0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110` (`0x36615Cf349d7F6344891B1e7CA7C72883F5dc049`)

See `node/sequencer/config.rs` for config options and defaults. Use env variables to override, e.g.:

```
prover_api_fake_provers_enabled=false cargo run --release
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
anvil --load-state zkos-l1-state.json --port 8545
```

### Otterscan

Server supports `ots_` namespace and hence can be used in combination with [Otterscan](https://github.com/otterscan/otterscan)
block explorer. To run a local instance as a Docker container (bound to `http://localhost:5100`):

```
docker run --rm -p 5100:80 --name otterscan -d --env ERIGON_URL="http://127.0.0.1:3050" otterscan/otterscan
```

See Otterscan's [docs](https://docs.otterscan.io/intro/) for other running options.

## Design

### Subsystems

* **Sequencer** subsystem — mandatory for every node. Executes transactions in VM, sends results downstream to other
  components.
    * Handles `Produce` and `Replay` commands in an uniform way (see `model/mod.rs` and `execution/block_executor.rs`)
    * For each block: (1) persists it in WAL (see `block_replay_storage.rs`), (2) pushes to `state` (see `state`
      crate), (3) exposes the block and tx receipts to API (see `repositories/mod.rs`), (4) pushes to async channels for
      downstream subsystems. Waits on backpressure.
* **API** subsystem — optional (not configurable atm). Has shared access to `state`. Exposes ethereum-compatible JSON
  RPC
* **Batcher** subsystem — optional (configurable via `batcher_component_enabled=true/false`). Has shared access to
  `state`.
    * Turns a stream of blocks into a stream of batches (1 batch = 1 proof = 1 L1 commit); exposes Prover APIs; submits
      batches and proofs to L1.
      Note: currently 1 block == 1 batch, multi-block batches are not implemented yet.
    * For each batch, computes the Prover Input (runs RiscV binary (`app.bin`) and records its input as a stream of
      `Vec<u32>` - see `batcher/mod.rs`)
    * This process requires Merkle Tree with materialized root hashes and proofs at every block boundary (not batch!).
      This will be optimized later on.
    * if `localhost:8545` is available, runs **L1 sender**.

Note on **Persistent Tree** — it is only necessary for Batcher Subsystem. Sequencer doesn't need the tree — block hashes
don't include root hash. Still, even when batcher subsystem is not enabled, we want to run the tree for potential
failover.

### Component Details

<img width="1549" height="658" alt="Screenshot 2025-07-24 at 15 00 55" src="https://github.com/user-attachments/assets/e1a472bd-d14b-4840-bd89-223347bebccf" />

See individual components and state recovery details in the table below. Note that most components have little to no
internal state or persistence — this is one of the design principles.

| Component                              | In-memory state                                                                                              | Persistence                                                                                                          | State Recovery                                                                                                                                                                                                                                                                                                                            | 
|----------------------------------------|--------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------| 
| **Command Source**                     | `starting_block` (only used on startup)                                                                      | none                                                                                                                 | `starting_block` is the first block **after** the compacted block stored in `state`, i.e., `starting_block = highest_block - blocks_to_retain_in_memory`.                                                                                                                                                                                 | 
| **BlockContextProvider**               | `next_l1_priority_id`; `block_hashes_for_next_block` (last 256 block hashes)                                 | none                                                                                                                 | `next_l1_priority_id`: take from `ReplayRecord` of `starting_block - 1`; `block_hashes_for_next_block`: take from 256 `ReplayRecord`s before `starting_block`                                                                                                                                                                             |
| **L1Watcher**                          | Gapless list of Priority transactions - starting from the last committed to L1                               | none                                                                                                                 | none - recovers itself from L1                                                                                                                                                                                                                                                                                                            |
| **L2Mempool** _(RETH crate)_           | prepared list of pending L2 transactions                                                                     | none                                                                                                                 | none (consider persisting mempool transactions in the future)                                                                                                                                                                                                                                                                             |
| **BlockExecutor**                      | none 🔥                                                                                                      | none                                                                                                                 | none                                                                                                                                                                                                                                                                                                                                      |
| **Repositories** (API subsystem)       | BlockHeaders and Transactions for ~`blocks_to_retain_in_memory` blocks                                       | Historical BlockHeaders and Transactions                                                                             | none - recovers naturally when replaying blocks from `starting_block`                                                                                                                                                                                                                                                                     |
| **State**                              | All Storage Logs and Preimages for `blocks_to_retain_in_memory` last blocks                                  | Compacted state at some older block (`highest_block - blocks_to_retain_in_memory`): full state map and all preimages | none - recovers naturally when replaying blocks from `starting_block`                                                                                                                                                                                                                                                                     |
| **Merkle Tree**                        | Only persistence                                                                                             | Full Merkle tree - including previous values on each leaf                                                            | none                                                                                                                                                                                                                                                                                                                                      |
| ⬇️ _Batcher Subsystem Components_      | ⬇️ _Components below operate on Batches - not Blocks️_                                                       | ⬇️ _Components below must not rely on persistence - otherwise failover is not possible_                              | ⬇️                                                                                                                                                                                                                                                                                                                                        |                              
| **Batcher**                            | startup: `starting_batch` and `batcher_starting_block`; <br/> operation: Trailing Batch's `CommitBatchInfo`; | none                                                                                                                 | `first_block_to_process`: block **after** the last block in the last committed L1 batch;<br/> `last_persisted_block`: the block after which we start checking for batch timeouts <br/> `StoredBatchInfo` in `run_loop`: Currently: from FRI cache; todo - Load last committed `StoredBatchInfo` from L1 OR reprocess last committed batch |                             
| **Prover Input Generator**             |                                                                                                              | none                                                                                                                 | none                                                                                                                                                                                                                                                                                                                                      |                             
| **FRI Job Manager**                    | Gapless List of unproved batches with `ProverInput` and prover assignment info                               | none                                                                                                                 | none - batches before `starting_batch` are guaranteed to have FRI proofs, batches after will go through the pipeline again                                                                                                                                                                                                                |                             
| **FRI Store/Cache**                    | none                                                                                                         | `Map<BatchNumber, FRIProof>` (todo: extract from the node process to enable failover)                                | none                                                                                                                                                                                                                                                                                                                                      |                             
| **L1 Committer**                       | none*                                                                                                        | none                                                                                                                 | none - recovers itself from L1                                                                                                                                                                                                                                                                                                            |                             
| **SNARK Job Manager** (TODO - missing) | Gapless list of batches with their FRI proofs and prover assignment info                                     | none                                                                                                                 | Load batches that are committed but not proved on L1 yet. Load their FRI proofs from FRI cache (TODO)                                                                                                                                                                                                                                     |                             
| **L1 Executor** (TODO - missing)       | none* (TODO: tree for `PriorityOpsBatchInfo`)                                                                | none                                                                                                                 | none - recovers itself from L1                                                                                                                                                                                                                                                                                                            |                             

### RPC

* All standard `eth_` methods are supported (except those specific to EIP-2930, EIP-4844 and EIP-7702). Block tags have a special meaning:
  * `earliest` - not supported yet (will return genesis or first uncompressed block)
  * `pending` - the latest produced block
  * `latest` - same as `pending` (consider taking consensus into account here)
  * `safe` - the latest block that has been committed to L1
  * `finalized` - not supported yet (will return the latest block that has been executed on L1)
* `zks_` namespace is kept to the minimum right now to avoid legacy from Era. Only following methods are supported:
  * `zks_getBridgehubContract`
* `ots_` namespace is used for Otterscan integration (meant for local development only)

### Prover API

```
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/available", get(list_available_proofs))
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
```

Note that it's the same api as in old sequencer integration, so FRI GPU Provers themselves are fully compatible.

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

* lacking seal criteria - potential for unprovable blocks
* older blocks are not available for eth_call
* no upgrade/interop logic

## Glossary

* `Block` vs `Batch`:
    * One `block` = one vm run in block_executor = one block_receipt,
    * one `batch` = one FRI proof = one L1 commit.

## L1 State

Please see the [How to run with L1 doc](docs/running_with_l1.md)
