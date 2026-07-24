use std::marker::PhantomData;
use std::time::Duration;
use zksync_os_operator_signer::SignerConfig;

/// Default confirmations required when settling directly on L1.
pub const DEFAULT_REQUIRED_CONFIRMATIONS_L1: u64 = 3;
/// Default max submission attempts per L1 transaction when the node rejects it with a
/// nonce-class error.
pub const DEFAULT_NONCE_ERROR_MAX_ATTEMPTS: usize = 10;
/// Default backoff between attempts after a nonce-class rejection.
pub const DEFAULT_NONCE_ERROR_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Configuration of L1 sender.
#[derive(Clone, Debug)]
pub struct L1SenderConfig<Input> {
    /// Operator signer configuration.
    /// Depending on the mode, this can be a commit/prove/execute operator.
    /// Supports both local private keys and GCP KMS keys.
    pub operator_signer: SignerConfig,

    /// Fee caps and replacement multipliers for L1 transactions.
    pub fee_config: L1SenderFeeConfig,

    /// Whether to skip in-flight recovery and replace pending L1 transactions on startup.
    pub force_transaction_resubmission: bool,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    pub poll_interval: Duration,

    /// Maximum time to wait for a transaction to be included on L1.
    pub transaction_timeout: Duration,

    /// Settlement-layer blocks (inclusive of the inclusion block) before a transaction is confirmed.
    pub required_confirmations: u64,

    /// Max submission attempts per L1 transaction when the node rejects it with a nonce-class
    /// error.
    pub nonce_error_max_attempts: usize,

    /// Backoff before retrying after a nonce-class rejection. Gives the node time to settle
    /// its pool/state view after a block import; the retry re-sends the same nonce.
    pub nonce_error_retry_backoff: Duration,

    pub phantom_data: PhantomData<Input>,
}

/// Fee configuration for L1 sender transactions.
#[derive(Clone, Copy, Debug)]
pub struct L1SenderFeeConfig {
    /// Max fee per gas we are willing to spend (in wei).
    pub max_fee_per_gas_wei: u128,

    /// Max priority fee per gas we are willing to spend (in wei).
    pub max_priority_fee_per_gas_wei: u128,

    /// Max fee per blob gas we are willing to spend (in wei).
    pub max_fee_per_blob_gas_wei: u128,

    /// Multiplier applied to `max_fee_per_gas_wei` when forcing transaction resubmission.
    pub max_fee_per_gas_replacement_multiplier: f64,

    /// Multiplier applied to `max_priority_fee_per_gas_wei` when forcing transaction resubmission.
    pub max_priority_fee_per_gas_replacement_multiplier: f64,

    /// Multiplier applied to `max_fee_per_blob_gas_wei` when forcing transaction resubmission.
    pub max_fee_per_blob_gas_replacement_multiplier: f64,
}
