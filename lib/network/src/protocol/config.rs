use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::wire::replays::RecordOverride;
use alloy::primitives::{Address, BlockNumber};
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};
use zksync_os_storage_api::ReplayRecord;

/// Dependencies required to run the main-node side of the `zks` protocol.
#[derive(Debug, Clone)]
pub struct MainNodeProtocolConfig {
    /// Accepted verifier signers for this main node.
    pub accepted_verifier_signers: Vec<Address>,
    /// Channel used to forward batch verification results back into the node.
    pub verify_result_tx: mpsc::Sender<PeerVerifyBatchResult>,
}

/// Dependencies required to run the external-node side of the `zks` protocol.
#[derive(Debug, Clone)]
pub struct ExternalNodeProtocolConfig {
    /// Block number to start streaming from.
    pub starting_block: Arc<RwLock<BlockNumber>>,
    /// All overrides to pass through when requesting records.
    pub record_overrides: Vec<RecordOverride>,
    /// Channel used to forward replay records into the local sequencer.
    pub replay_sender: mpsc::Sender<ReplayRecord>,
    /// Optional verifier configuration. When absent, this EN only participates in replay sync.
    pub verification: Option<ExternalNodeVerifierConfig>,
}

/// Verifier identity used by an external node when opting into verifier-role authentication.
#[derive(Debug, Clone)]
pub struct ExternalNodeVerifierConfig {
    pub signing_key: secrecy::SecretString,
    pub verify_batch_tx: mpsc::Sender<PeerVerifyBatch>,
    pub outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

/// Role-specific protocol configuration for the `zks` subprotocol.
pub enum ZksProtocolConfig {
    MainNode(MainNodeProtocolConfig),
    ExternalNode(ExternalNodeProtocolConfig),
}
