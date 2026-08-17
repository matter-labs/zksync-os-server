use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::wire::replays::RecordOverride;
use alloy::primitives::{Address, BlockNumber};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use zksync_os_storage_api::ReplayRecord;

/// Network dependencies for the main-node role.
///
/// These fields configure the `zks_2fa` handler. The replay-only `zks` handler receives its
/// storage dependency separately.
#[derive(Debug, Clone)]
pub struct MainNodeProtocolConfig {
    /// Accepted verifier signers for this main node.
    pub accepted_verifier_signers: Vec<Address>,
    /// Channel used to forward batch verification results back into the node.
    pub verify_result_tx: mpsc::Sender<PeerVerifyBatchResult>,
}

/// Network dependencies for the external-node role.
///
/// Replay fields configure the `zks` handler. Setting [`Self::verification`] also registers the
/// optional `zks_2fa` handler.
#[derive(Debug, Clone)]
pub struct ExternalNodeProtocolConfig {
    /// Block number to start streaming from.
    pub starting_block: Arc<RwLock<BlockNumber>>,
    /// All overrides to pass through when requesting records.
    pub record_overrides: Vec<RecordOverride>,
    /// Maximum replay records requested per `BlockReplays` response message.
    pub max_blocks_per_message: u64,
    /// Channel used to forward replay records into the local sequencer.
    pub replay_sender: mpsc::Sender<ReplayRecord>,
    /// Optional verifier configuration used to register `zks_2fa` alongside replay sync.
    pub verification: Option<ExternalNodeVerifierConfig>,
    /// Maximum time to wait without receiving any message on an established replay connection
    /// before reporting it as stalled (which disconnects the peer and forces a fresh session).
    /// Must comfortably exceed the chain's block cadence.
    pub replay_inactivity_timeout: Duration,
}

/// Verifier identity and channels used by an external node participating in `zks_2fa`.
#[derive(Debug, Clone)]
pub struct ExternalNodeVerifierConfig {
    pub signing_key: secrecy::SecretString,
    pub verify_batch_tx: mpsc::Sender<PeerVerifyBatch>,
    pub outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

/// Role-specific configuration used to register the `zks` and optional `zks_2fa` handlers.
#[derive(Debug, Clone)]
pub enum ZksProtocolConfig {
    MainNode(MainNodeProtocolConfig),
    ExternalNode(ExternalNodeProtocolConfig),
}
