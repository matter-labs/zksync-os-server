use alloy::primitives::{B256, TxHash};
use std::collections::HashSet;
use std::fmt::Display;
use std::time::Duration;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_mempool::MarkingTxStream;
use zksync_os_pipeline::HasBlockRangeEnd;
use zksync_os_storage_api::BlockContext;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{BlockOutput, BlockStartCursors, ProtocolSemanticVersion};

/// Block output with additional information about storage slots read during execution.
#[derive(Debug)]
pub struct BlockOutputWithReads {
    inner: BlockOutput,
    /// Keys read, but not written during block execution.
    read_keys: HashSet<B256>,
}

impl BlockOutputWithReads {
    pub(crate) fn new(inner: BlockOutput, mut read_keys: HashSet<B256>) -> Self {
        for write in &inner.storage_writes {
            read_keys.remove(&write.key);
        }
        // Reclaim unnecessary capacity; the read keys are immutable after this point.
        read_keys.shrink_to_fit();

        Self { inner, read_keys }
    }

    /// Returns block output + keys read, but not written during block execution.
    pub fn into_parts(self) -> (BlockOutput, HashSet<B256>) {
        (self.inner, self.read_keys)
    }

    pub(crate) fn inner_mut(&mut self) -> &mut BlockOutput {
        &mut self.inner
    }

    pub(crate) fn read_keys(&self) -> &HashSet<B256> {
        &self.read_keys
    }
}

impl AsRef<BlockOutput> for BlockOutputWithReads {
    fn as_ref(&self) -> &BlockOutput {
        &self.inner
    }
}

/// `BlockCommand`s drive the sequencer execution.
/// Produced by `CommandProducer` - first blocks are `Replay`ed from block replay storage
/// and then `Produce`d indefinitely.
///
/// Downstream transform:
/// `BlockTransactionProvider: (L1Mempool/L1Watcher, L2Mempool, BlockCommand) -> (PreparedBlockCommand)`
#[derive(Clone, Debug)]
pub enum BlockCommand {
    /// Replay a block from block replay storage.
    Replay(Box<ReplayRecord>),
    /// Produce a new block from the mempool.
    Produce(ProduceCommand),
    /// Rebuild an existing block.
    Rebuild(Box<RebuildCommand>),
}

/// Type of the block command.
#[derive(Debug, Clone, Copy)]
pub enum BlockCommandType {
    Replay,
    Produce,
    Rebuild,
}

/// Message flowing from `BlockExecutor` → `BlockCanonizer` → `BlockApplier`.
#[derive(Debug)]
pub struct BlockPayload {
    pub output: BlockOutputWithReads,
    pub record: ReplayRecord,
    pub command_type: BlockCommandType,
    /// L2 txs the VM rejected during block building (purged from mempool).
    /// Surfaced so RPC subscribers can report a reason instead of timing out.
    pub failed_transactions: Vec<(TxHash, InvalidTransaction)>,
}

impl HasBlockRangeEnd for BlockPayload {
    fn block_number(&self) -> u64 {
        self.record.block_context.block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.record.block_context.timestamp)
    }
}

/// Message flowing from `BlockApplier` → `TreeManager`.
#[derive(Debug)]
pub struct AppliedBlock {
    pub output: BlockOutputWithReads,
    pub record: ReplayRecord,
}

impl HasBlockRangeEnd for AppliedBlock {
    fn block_number(&self) -> u64 {
        self.record.block_context.block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.record.block_context.timestamp)
    }
}

/// Command to produce a new block.
#[derive(Clone, Debug)]
pub struct ProduceCommand;

/// Command to rebuild existing block.
#[derive(Clone, Debug)]
pub struct RebuildCommand {
    pub replay_record: ReplayRecord,
    pub make_empty: bool,
    pub reset_timestamp: bool,
}

impl BlockCommand {
    pub fn command_type(&self) -> BlockCommandType {
        match self {
            BlockCommand::Replay(_) => BlockCommandType::Replay,
            BlockCommand::Produce(_) => BlockCommandType::Produce,
            BlockCommand::Rebuild(_) => BlockCommandType::Rebuild,
        }
    }
}

impl Display for BlockCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockCommand::Replay(record) => write!(
                f,
                "Replay block {} ({} txs); starting l1 priority id: {}",
                record.block_context.block_number,
                record.transactions.len(),
                record.starting_cursors.l1_priority_id,
            ),
            BlockCommand::Produce(_) => write!(f, "Produce block"),
            BlockCommand::Rebuild(command) => write!(
                f,
                "Rebuild block {} ({} txs);",
                command.replay_record.block_context.block_number,
                command.replay_record.transactions.len(),
            ),
        }
    }
}

/// BlockCommand + Tx Source = PreparedBlockCommand
/// We use `BlockCommand` upstream (`CommandProducer`, `BlockTransactionProvider`),
/// while doing all preparations that depend on command type (replay vs produce).
/// Then we switch to `PreparedBlockCommand` in `BlockExecutor`,
/// which should handle them uniformly.
///
/// Downstream transform:
/// `BlockExecutor: (State, PreparedBlockCommand) -> (BlockOutput, ReplayRecord)`
pub struct PreparedBlockCommand<'a> {
    pub block_context: BlockContext,
    pub seal_policy: SealPolicy,
    pub invalid_tx_policy: InvalidTxPolicy,
    pub tx_source: MarkingTxStream<'a>,
    pub metrics_label: &'static str,
    pub protocol_version: ProtocolSemanticVersion,
    /// Expected hash of the block output (missing for command generated from `BlockCommand::Produce`)
    pub expected_block_output_hash: Option<B256>,
    pub previous_block_timestamp: u64,
    /// Contract preimages to be included before the block execution.
    /// Can be non-empty e.g. when processing upgrade transactions.
    pub force_preimages: Vec<(B256, Vec<u8>)>,
    /// Whether the sequencer expects exactly one `SetSLChainId` system tx to execute immediately
    /// after an upgrade tx before the block is sealed.
    pub expect_sl_chain_id_tx_after_upgrade: bool,
    /// L1 watcher cursors at the start of this block.
    pub starting_cursors: BlockStartCursors,
    pub interop_roots_per_block: u64,
    /// Whether canonical state transition should strictly consume executed txs from live subpools.
    /// `true` for produced blocks, `false` for replay/rebuild.
    pub strict_subpool_cleanup: bool,
}

/// Behaviour when VM returns an InvalidTransaction error.
#[derive(Clone, Copy, Debug)]
pub enum InvalidTxPolicy {
    /// Invalid tx is skipped in block; optionally marked as invalid in the tx source.
    /// During block rebuild we keep `mark_in_source = false`, because rebuild should only
    /// reprocess historical blocks and must not affect the current tx source state.
    RejectAndContinue { mark_in_source: bool },
    /// Bubble the error up and abort the whole block. Used when replaying a block (ReplayLog / Replica / EN)
    Abort,
}

#[derive(Clone, Copy, Debug)]
pub enum SealPolicy {
    /// Seal non-empty blocks after deadline or N transactions. Used when producing a block
    /// (Block Deadline, Block Size)
    Decide(Duration, usize),
    /// Seal when all txs from tx source are executed.
    /// `allowed_to_finish_early` indicates whether it's expected for block to be sealed earlier for different reason.
    /// - `Replay` maps to `UntilExhausted { allowed_to_finish_early: false }`
    /// - `Rebuild` maps to `UntilExhausted { allowed_to_finish_early: true }`
    UntilExhausted { allowed_to_finish_early: bool },
}
