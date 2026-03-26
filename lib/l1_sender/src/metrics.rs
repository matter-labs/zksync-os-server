use crate::commands::SendToL1;
use alloy::primitives::utils::{format_ether, format_units};
use alloy::providers::utils::Eip1559Estimation;
use alloy::rpc::types::TransactionReceipt;
use vise::{Buckets, Counter, EncodeLabelValue, Gauge, Histogram, LabeledFamily, Metrics};
use zksync_os_observability::{GenericComponentState, StateLabel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "seal_reason", rename_all = "snake_case")]
pub enum L1SenderState {
    WaitingRecv,
    WaitingSend,
    SendingToL1,
    WaitingL1Inclusion,
    /// Network gas fees exceed the configured cap; waiting for congestion to pass.
    GasBlocked,
    /// Blob base fee exceeds the configured cap; waiting for blob demand to drop.
    BlobFeeBlocked,
    /// Retrying after a transient RPC error with exponential backoff.
    TransientBackoff,
}

impl StateLabel for L1SenderState {
    fn generic(&self) -> GenericComponentState {
        match self {
            L1SenderState::WaitingRecv => GenericComponentState::WaitingRecv,
            L1SenderState::WaitingSend => GenericComponentState::WaitingSend,
            L1SenderState::SendingToL1 => GenericComponentState::Processing,
            L1SenderState::WaitingL1Inclusion => GenericComponentState::Processing,
            // Blocked/backoff states are still "processing" in the generic view —
            // the component is alive and making progress decisions, just not sending.
            L1SenderState::GasBlocked => GenericComponentState::Processing,
            L1SenderState::BlobFeeBlocked => GenericComponentState::Processing,
            L1SenderState::TransientBackoff => GenericComponentState::Processing,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            L1SenderState::WaitingRecv => "waiting_recv",
            L1SenderState::WaitingSend => "waiting_send",
            L1SenderState::SendingToL1 => "sending_to_l1",
            L1SenderState::WaitingL1Inclusion => "waiting_l1_inclusion",
            L1SenderState::GasBlocked => "gas_blocked",
            L1SenderState::BlobFeeBlocked => "blob_fee_blocked",
            L1SenderState::TransientBackoff => "transient_backoff",
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "l1_sender")]
pub struct L1SenderMetrics {
    /// Used to report L1 operator addresses to Prometheus (commit/prove/execute),
    /// Gauge is always set to one.
    #[metrics(labels = ["operation", "operator_address"])]
    pub l1_operator_address: LabeledFamily<(&'static str, &'static str), Gauge, 2>,

    /// Operator wallet balance
    #[metrics(labels = ["command"])]
    pub balance: LabeledFamily<&'static str, Gauge<f64>>,

    /// Number of L1 transactions being sent in one batch (in parallel) - see `command_limit` config param.
    #[metrics(labels = ["command"])]
    pub parallel_transactions: LabeledFamily<&'static str, Gauge<u64>>,

    /// L1 Transaction fee in Ether (i.e. total cost of commit/prove/execute)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(0.0001..=100.0, 3.0))]
    pub l1_transaction_fee_ether: LabeledFamily<&'static str, Histogram<f64>>,

    /// L1 Transaction fee in Ether per l2 transaction (`l1_transaction_fee / transactions_per_batch`)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(0.0001..=100.0, 3.0))]
    pub l1_transaction_fee_per_l2_tx_ether: LabeledFamily<&'static str, Histogram<f64>>,

    /// Total L1 gas used by L1 transaction (i.e. commit/prove/execute)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(1.0..=10_000_000.0, 3.0))]
    pub gas_used: LabeledFamily<&'static str, Histogram<u64>>,

    /// L1 blob base fee (EIP4844) of pending L1 block.
    /// Reported by server when sending blob L1 transactions.
    /// Value returned by `provider.get_blob_base_fee()`
    #[metrics()]
    pub blob_base_fee_gwei: Gauge<f64>,

    /// The price actually paid by the EIP4844 transactions per blob gas
    /// Taken from `blob_gas_price` field of `TransactionReceipt`
    #[metrics()]
    pub effective_blob_gas_price_gwei: Gauge<f64>,

    /// Total L1 blob gas used by L1 commit (EIP4844)
    /// Buckets: one blob is `131,072` gas - with these buckets we'll see how many blobs per tx we send
    /// Taken from `blob_gas_used` field of `TransactionReceipt`
    #[metrics(buckets = Buckets::linear(131_100.0..=1_311_000.0, 131_100.0))]
    pub blob_gas_used: Histogram<u64>,

    /// The gas price paid post-execution by the transaction (i.e. base fee + priority fee).
    /// Taken from `effective_gas_price` field of `TransactionReceipt`
    #[metrics(labels = ["command"])]
    pub effective_gas_price_gwei: LabeledFamily<&'static str, Gauge<f64>>,

    /// L1 max_fee_per_gas (EIP1559) in gwei - as returned by `Eip1559Estimation`.  Reported by server when sending L1 transactions.
    #[metrics()]
    pub estimated_max_fee_per_gas_gwei: Gauge<f64>,
    /// L1 max_priority_fee_per_gas (EIP1559) in gwei - as returned by `Eip1559Estimation`. Reported by server when sending L1 transactions.
    #[metrics()]
    pub estimated_max_priority_fee_per_gas_gwei: Gauge<f64>,

    /// L1 gas used by L1 transaction per l2 transaction (`gas_used / transactions_per_batch`)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(1.0..=10_000_000.0, 3.0))]
    pub gas_used_per_l2_tx: LabeledFamily<&'static str, Histogram<u64>>,

    /// Last nonce used
    #[metrics(labels = ["command"])]
    pub nonce: LabeledFamily<&'static str, Gauge<u64>>,

    /// Total number of transient RPC errors encountered in the send loop.
    /// Each increment corresponds to one backoff cycle.
    #[metrics()]
    pub transient_errors: Counter,

    /// Number of recoverable errors encountered, broken down by reason.
    /// Labels: "gas_blocked" | "blob_fee_blocked" | "tx_timeout" | "nonce_too_low"
    #[metrics(labels = ["reason"])]
    pub recoverable_errors: LabeledFamily<&'static str, Counter>,
}

impl L1SenderMetrics {
    /// Reports metrics from a successful L1 transaction receipt.
    ///
    /// Parses receipt fields into floats for Prometheus. Parse failures are logged
    /// at WARN level rather than propagated — a metrics formatting error should
    /// never affect the send loop.
    pub fn report_tx_receipt<Input: SendToL1>(&self, command: &Input, receipt: TransactionReceipt) {
        let l2_txs_count: usize = command
            .as_ref()
            .iter()
            .map(|envelope| envelope.batch.tx_count)
            .sum();
        let l1_transaction_fee = receipt.gas_used as u128 * receipt.effective_gas_price;

        let l1_transaction_fee_ether_per_l2_tx = l1_transaction_fee
            .checked_div(l2_txs_count as u128)
            .map(format_ether);
        tracing::info!(
            %command,
            tx_hash = ?receipt.transaction_hash,
            l1_block_number = receipt.block_number.unwrap(),
            gas_used = receipt.gas_used,
            gas_used_per_l2_tx = receipt.gas_used.checked_div(l2_txs_count as u64),
            l1_transaction_fee_ether = format_ether(l1_transaction_fee),
            l1_transaction_fee_ether_per_l2_tx,
            "succeeded on L1",
        );
        self.gas_used[&Input::NAME].observe(receipt.gas_used);
        if let Some(gas_used_per_l2_tx) = receipt.gas_used.checked_div(l2_txs_count as u64) {
            self.gas_used_per_l2_tx[&Input::NAME].observe(gas_used_per_l2_tx);
        }
        if let Some(blob_gas_used) = receipt.blob_gas_used {
            self.blob_gas_used.observe(blob_gas_used);
        }
        match format_ether(l1_transaction_fee).parse::<f64>() {
            Ok(v) => self.l1_transaction_fee_ether[&Input::NAME].observe(v),
            Err(e) => tracing::warn!(?e, "failed to parse l1_transaction_fee_ether for metrics"),
        }
        if let Some(l1_transaction_fee_per_l2_tx) = l1_transaction_fee_ether_per_l2_tx {
            match l1_transaction_fee_per_l2_tx.parse::<f64>() {
                Ok(v) => self.l1_transaction_fee_per_l2_tx_ether[&Input::NAME].observe(v),
                Err(e) => tracing::warn!(
                    ?e,
                    "failed to parse l1_transaction_fee_per_l2_tx for metrics"
                ),
            }
        }
        if let Ok(v) = Self::wei_to_gwei(receipt.effective_gas_price) {
            self.effective_gas_price_gwei[&Input::NAME].set(v);
        } else {
            tracing::warn!("failed to parse effective_gas_price for metrics");
        }
        if let Some(blob_gas_price) = receipt.blob_gas_price {
            if let Ok(v) = Self::wei_to_gwei(blob_gas_price) {
                self.effective_blob_gas_price_gwei.set(v);
            } else {
                tracing::warn!("failed to parse effective_blob_gas_price for metrics");
            }
        }
    }

    /// Reports EIP-1559 fee estimation metrics.
    ///
    /// Parse failures are logged at WARN level rather than propagated.
    pub fn report_l1_eip_1559_estimation(&self, eip1559_est: Eip1559Estimation) {
        if let Ok(v) = Self::wei_to_gwei(eip1559_est.max_fee_per_gas) {
            self.estimated_max_fee_per_gas_gwei.set(v);
        } else {
            tracing::warn!("failed to parse estimated_max_fee_per_gas for metrics");
        }
        if let Ok(v) = Self::wei_to_gwei(eip1559_est.max_priority_fee_per_gas) {
            self.estimated_max_priority_fee_per_gas_gwei.set(v);
        } else {
            tracing::warn!("failed to parse estimated_max_priority_fee_per_gas for metrics");
        }
    }

    /// Reports the current blob base fee metric.
    ///
    /// Parse failures are logged at WARN level rather than propagated.
    pub fn report_blob_base_fee(&self, base_fee_wei: u128) {
        if let Ok(v) = Self::wei_to_gwei(base_fee_wei) {
            self.blob_base_fee_gwei.set(v);
        } else {
            tracing::warn!("failed to parse blob_base_fee for metrics");
        }
    }

    fn wei_to_gwei(wei: u128) -> anyhow::Result<f64> {
        use anyhow::Context as _;
        format_units(wei, "gwei")
            .context("Failed to format wei value to gwei")?
            .parse::<f64>()
            .context("Failed to parse gwei value")
    }
}

#[vise::register]
pub static L1_SENDER_METRICS: vise::Global<L1SenderMetrics> = vise::Global::new();
