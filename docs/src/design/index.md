# Design principles

* Minimal, async persistence
    * to meet throughput and latency requirements, we avoid synchronous persistence at the critical path. Additionally,
      we aim at storing only the data that is strictly needed - minimizing the potential for state inconsistency
* Easy to replay arbitrary blocks
    * Sequencer: components are idempotent
    * Batcher: `batcher` component skips all blocks until the first uncommitted batch.
      Thus, downstream components only receive batches that they need to act upon
* State - strong separation between
    * Actual state - data needed to execute VM: key-value storage and preimages map
    * Receipts repositories - data only needed in API
    * Data related to Proofs and L1 - not needed by sequencer / JSON RPC - only introduced downstream from `batcher`
