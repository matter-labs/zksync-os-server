# Subsystems

* **Sequencer** subsystem — mandatory for every node. Executes transactions in VM, sends results downstream to other
  components.
    * Handles `Produce` and `Replay` commands in an uniform way (see `model/mod.rs` and `execution/block_executor.rs`)
    * For each block: (1) persists it in WAL (see `block_replay_storage.rs`), (2) pushes to `state` (see `state`
      crate), (3) exposes the block and tx receipts to API (see `repositories/mod.rs`), (4) pushes to async channels for
      downstream subsystems. Waits on backpressure.
* **API** subsystem — optional (not configurable atm). Has shared access to `state`. Exposes ethereum-compatible JSON
  RPC
* **Batcher** subsystem — runs for the main node - most of it is disabled for ENs.
    * Turns a stream of blocks into a stream of batches (1 batch = 1 proof = 1 L1 commit); exposes Prover APIs; submits
      batches and proofs to L1.
    * For each batch, computes the Prover Input (runs RiscV binary (`app.bin`) and records its input as a stream of
      `Vec<u32>` - see `batcher/mod.rs`)
    * This process requires Merkle Tree with materialized root hashes and proofs at every block boundary.
    * Runs L1 senders for each of `commit` / `prove` / `execute`
    * Runs Priority Tree Manager that applies new L1->L2 transactions to the dynamic Merkle tree and prepares `execute` commands.
      It's run both for main node and ENs. ENs don't send `execute` txs to L1, but they need to keep the tree up to date,
      so that if the node become main, it doesn't need to build the tree from scratch.

Note on **Persistent Tree** — it is only necessary for Batcher Subsystem. Sequencer doesn't need the tree — block hashes
don't include root hash. Still, even when batcher subsystem is not enabled, we want to run the tree for potential
failover.
