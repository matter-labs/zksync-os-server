use crate::model::{BatchEnvelope, FriProof};
use alloy::sol_types::SolCall;
use std::fmt::Display;

pub mod commit;
pub mod prove;

pub trait L1SenderCommand: Sized + Display {
    const NAME: &'static str;
    fn solidity_call(&self) -> impl SolCall;
    fn into_output_envelope(self) -> Vec<BatchEnvelope<FriProof>>;

    /// Only used for logging - as we send commands in batches,
    /// it's easier to print a single description for the whole batch.
    /// Note that one L1SenderCommand is still always a single L1 transaction.
    /// Do not include command name in the description.
    fn display_vec(input: &[Self]) -> String;
}
