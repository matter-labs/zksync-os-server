use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use alloy::primitives::Address;
use tokio::sync::{broadcast, mpsc};

/// Dependencies required to run the main-node side of the `zks_2fa` subprotocol.
#[derive(Debug, Clone)]
pub struct MainNode2faConfig {
    /// Accepted verifier signers for this main node.
    pub accepted_verifier_signers: Vec<Address>,
    /// Channel used to forward batch verification results back into the node.
    pub verify_result_tx: mpsc::Sender<PeerVerifyBatchResult>,
}

/// Verifier identity and transport used by an external node participating in `zks_2fa`.
#[derive(Debug, Clone)]
pub struct ExternalNode2faConfig {
    pub signing_key: secrecy::SecretString,
    pub verify_batch_tx: mpsc::Sender<PeerVerifyBatch>,
    pub outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}
