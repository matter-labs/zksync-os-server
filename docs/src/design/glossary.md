## Glossary

* `Block` vs `Batch`:
    * One `block` = one vm run in block_executor = one block_receipt,
    * one `batch` = one FRI proof = one L1 commit.

* `Priority operation` — an L1→L2 transaction initiated on L1 (e.g. a deposit).
  Priority ops are ordered by an on-chain priority queue and mirrored into the
  priority tree so their inclusion can be proven against L1.

* `Replay record` — the persisted, deterministic description of a block
  (context, transactions, protocol version) that lets a node re-execute the
  block and reproduce the exact same output. Replay records are the source of
  truth used during recovery and by external nodes.

* `Sequencer` — the component that orders transactions and produces blocks.
  Only the main node sequences; external nodes replay the sequencer's output.

* `External Node (EN)` — a node that does not sequence. It replays blocks and
  batches produced by the main node and serves reads.

* `Pubdata` — the data published so that L2 state can be reconstructed from L1.
  Depending on configuration it is posted via calldata or blobs.

* `Preimage` — the full byte contents behind a hash committed in state.
  Some preimages are force-published so that state can be reconstructed.

* `Canonization` — the step that finalizes a locally produced block into the
  canonical chain (assigning it its canonical hash and gossiping it to peers).
