use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Address, Bytes};
use itertools::Itertools;
use std::fmt::Display;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_pipeline::HasBlockRangeEnd;

pub mod commit;
pub mod execute;
pub mod prove;

/// Batches that are already committed/proved may also go through the pipeline.
/// For such batches, a Passthrough variant is generated.
/// For batches that have to be processed on L1, a SendToL1 variant is used.
pub enum L1SenderCommand<Command: SendToL1> {
    SendToL1(Command),
    Passthrough(Box<SignedBatchEnvelope<FriProof>>),
}

impl<C: SendToL1> L1SenderCommand<C> {
    pub fn first_batch_number(&self) -> u64 {
        match self {
            Self::SendToL1(cmd) => cmd.as_ref()[0].batch_number(),
            Self::Passthrough(envelope) => envelope.batch_number(),
        }
    }

    pub fn last_batch_number(&self) -> u64 {
        match self {
            Self::SendToL1(cmd) => cmd.as_ref().last().unwrap().batch_number(),
            Self::Passthrough(envelope) => envelope.batch_number(),
        }
    }

    pub fn batch_count(&self) -> usize {
        match self {
            Self::SendToL1(cmd) => cmd.as_ref().len(),
            Self::Passthrough(_) => 1,
        }
    }

    fn last_block(&self) -> &SignedBatchEnvelope<FriProof> {
        match self {
            Self::SendToL1(cmd) => cmd.as_ref().last().unwrap(),
            Self::Passthrough(envelope) => envelope,
        }
    }

    /// Last block number in this command's final batch.
    pub fn last_block_number(&self) -> u64 {
        self.last_block().batch.last_block_number
    }

    /// Timestamp of the last block in this command's final batch.
    pub fn block_timestamp(&self) -> Option<u64> {
        Some(self.last_block().batch.batch_info.last_block_timestamp)
    }
}

impl<C: SendToL1 + Send + 'static> HasBlockRangeEnd for L1SenderCommand<C> {
    fn block_number(&self) -> u64 {
        self.last_block_number()
    }
    fn block_timestamp(&self) -> Option<u64> {
        self.last_block().block_timestamp()
    }
    fn batch_number(&self) -> Option<u64> {
        Some(self.last_batch_number())
    }
}

pub trait SendToL1:
    Into<Vec<SignedBatchEnvelope<FriProof>>>
    + AsRef<[SignedBatchEnvelope<FriProof>]>
    + AsMut<[SignedBatchEnvelope<FriProof>]>
    + Display
{
    const COMPONENT_ID: zksync_os_pipeline::ComponentId;
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    const PASSTHROUGH_STAGE: BatchExecutionStage;
    /// We use `Bytes` instead of `SolCall`, because SolCall is a trait that cannot be dyn
    fn solidity_call(&self, gateway: bool, operator: &Address) -> Bytes;

    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> {
        None
    }

    /// Only used for logging - as we send commands in bulk, it's natural to print a single range
    /// for the whole group, e.g. "1-3, 4, 5-6" instead of "1, 2, 3, 4, 5, 6"
    /// Note that one `L1SenderCommand` is still always a single L1 transaction.
    fn display_range(cmds: &[Self]) -> String {
        cmds.iter()
            .map(|cmd| {
                let envelopes = cmd.as_ref();
                // Safe unwraps as each command contains at least one envelope
                let first = envelopes.first().unwrap().batch_number();
                let last = envelopes.last().unwrap().batch_number();
                if first == last {
                    format!("{first}")
                } else {
                    format!("{first}-{last}")
                }
            })
            .join(", ")
    }
}
