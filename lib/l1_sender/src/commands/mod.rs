use crate::model::{BatchEnvelope, FriProof};
use alloy::sol_types::SolCall;

pub mod commit;
pub mod prove;

pub trait L1SenderCommand: Sized {
    const NAME: &'static str;
    fn solidity_call(&self) -> impl SolCall;
    fn into_output_envelope(self) -> Vec<BatchEnvelope<FriProof>>;

    /// Only used for logging
    fn short_description(&self) -> String;

    /// Only used for logging - as we send commands in batches,
    /// it's easier to print a single description for the whole batch.
    /// Note that one L1SenderCommand is still always a single L1 transaction.
    fn vec_fmt_debug(input: &[Self]) -> String;
}
