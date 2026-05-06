use crate::GenericComponentState;
use vise::{Counter, Gauge, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
pub struct GeneralMetrics {
    /// Counts the number of seconds spent in each state.
    /// `specific_state` tracks component-specific state -
    /// the set of values may be different for different components
    #[metrics(labels = ["component", "generic_state", "specific_state"])]
    pub component_time_spent_in_state:
        LabeledFamily<(&'static str, GenericComponentState, &'static str), Counter<f64>, 3>,

    /// Unix timestamp for when the process was started.
    /// Additionally, labels are used to track the version and role (main node / external node)
    #[metrics(labels = ["version", "role"])]
    pub process_started_at: LabeledFamily<(&'static str, &'static str), Gauge<i64>, 2>,

    /// Time spent on various startup routines.
    #[metrics(labels = ["stage"])]
    pub startup_time: LabeledFamily<&'static str, Gauge<f64>>,

    #[metrics(labels = ["fee_collector_address"])]
    pub fee_collector_address: LabeledFamily<&'static str, Gauge, 1>,

    pub chain_id: Gauge<u64>,

    /// Number of blacklisted addresses in the internal config on server startup
    pub blacklisted_addresses_count: Gauge<usize>,

    // -- Block seal criteria (from `SequencerConfig`) ----------------------------------------
    /// Block seal criterion: deadline since first tx after which a block is sealed.
    #[metrics(unit = Unit::Seconds)]
    pub block_seal_block_time: Gauge<f64>,
    /// Block seal criterion: max number of transactions per block.
    pub block_seal_max_transactions_in_block: Gauge<u64>,
    /// Block seal criterion: max gas used per block.
    pub block_seal_block_gas_limit: Gauge<u64>,
    /// Block seal criterion: max pubdata bytes per block.
    #[metrics(unit = Unit::Bytes)]
    pub block_seal_block_pubdata_limit: Gauge<u64>,

    // -- Batch seal criteria (from `BatcherConfig`) ------------------------------------------
    /// Batch seal criterion: max time a batch stays open before sealing.
    #[metrics(unit = Unit::Seconds)]
    pub batch_seal_batch_timeout: Gauge<f64>,
    /// Batch seal criterion: max number of transactions per batch.
    pub batch_seal_tx_per_batch_limit: Gauge<u64>,
    /// Batch seal criterion: max number of interop roots per batch.
    pub batch_seal_interop_roots_per_batch_limit: Gauge<u64>,

    // -- RPC request limits (from `RpcConfig`) -----------------------------------------------
    /// RPC limit: gas limit for transactions executed via `eth_call` / `eth_estimateGas`.
    pub rpc_eth_call_gas: Gauge<u64>,
    /// RPC limit: max concurrent API connections (HTTP and WS).
    pub rpc_max_connections: Gauge<u64>,
    /// RPC limit: max RPC request payload size for both HTTP and WS.
    #[metrics(unit = Unit::Bytes)]
    pub rpc_max_request_size: Gauge<u64>,
    /// RPC limit: max RPC response payload size for both HTTP and WS.
    #[metrics(unit = Unit::Bytes)]
    pub rpc_max_response_size: Gauge<u64>,
    /// RPC limit: max number of blocks that can be scanned per filter.
    pub rpc_max_blocks_per_filter: Gauge<u64>,
    /// RPC limit: max number of logs returned in a single response.
    pub rpc_max_logs_per_response: Gauge<u64>,
    /// RPC limit: duration since the last filter poll after which the filter is considered stale.
    #[metrics(unit = Unit::Seconds)]
    pub rpc_stale_filter_ttl: Gauge<f64>,
    /// RPC limit: default timeout for `eth_sendRawTransactionSync`.
    #[metrics(unit = Unit::Seconds)]
    pub rpc_send_raw_transaction_sync_timeout: Gauge<f64>,
    /// RPC: factor applied to the pending block base fee returned by `eth_gasPrice`.
    pub rpc_gas_price_scale_factor: Gauge<f64>,
    /// RPC: pubdata price multiplier used during gas limit estimation (`eth_estimateGas`).
    pub rpc_estimate_gas_pubdata_price_factor: Gauge<f64>,
}

#[vise::register]
pub static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();
