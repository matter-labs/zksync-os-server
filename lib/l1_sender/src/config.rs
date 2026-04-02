use std::marker::PhantomData;
use std::time::Duration;
use zksync_os_operator_signer::SignerConfig;

/// Configuration of L1 sender.
#[derive(Debug)]
pub struct L1SenderConfig<Input> {
    /// Operator signer configuration.
    /// Depending on the mode, this can be a commit/prove/execute operator.
    /// Supports both local private keys and GCP KMS keys.
    pub operator_signer: SignerConfig,

    /// Max fee per gas we are willing to spend (in wei).
    pub max_fee_per_gas_wei: u128,

    /// Max priority fee per gas we are willing to spend (in wei).
    pub max_priority_fee_per_gas_wei: u128,

    /// Max fee per blob gas we are willing to spend (in wei).
    pub max_fee_per_blob_gas_wei: u128,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    pub poll_interval: Duration,

    /// Maximum time to wait for a transaction to be included on L1 before attempting
    /// resubmission with updated gas fees.
    pub transaction_timeout: Duration,

    /// Use Fusaka blob transaction format if the timestamp has passed.
    pub fusaka_upgrade_timestamp: u64,

    pub phantom_data: PhantomData<Input>,
}

impl<Input> Clone for L1SenderConfig<Input> {
    fn clone(&self) -> Self {
        Self {
            operator_signer: self.operator_signer.clone(),
            max_fee_per_gas_wei: self.max_fee_per_gas_wei,
            max_priority_fee_per_gas_wei: self.max_priority_fee_per_gas_wei,
            max_fee_per_blob_gas_wei: self.max_fee_per_blob_gas_wei,
            command_limit: self.command_limit,
            poll_interval: self.poll_interval,
            transaction_timeout: self.transaction_timeout,
            fusaka_upgrade_timestamp: self.fusaka_upgrade_timestamp,
            phantom_data: PhantomData,
        }
    }
}
