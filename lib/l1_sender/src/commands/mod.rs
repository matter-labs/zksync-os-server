use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use alloy::sol_types::SolCall;
use itertools::Itertools;
use std::fmt::Display;

pub mod commit;
pub mod execute;
pub mod prove;

pub trait L1SenderCommand:
    Into<Vec<BatchEnvelope<FriProof>>>
    + AsRef<[BatchEnvelope<FriProof>]>
    + AsMut<[BatchEnvelope<FriProof>]>
    + Display
{
    const NAME: &'static str;
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    fn solidity_call(&self) -> impl SolCall;

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

    fn pubdata(&self) -> Vec<u8>;
}
