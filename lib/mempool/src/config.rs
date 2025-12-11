pub struct TxValidatorConfig {
    /// Max input size of a transaction to be accepted by mempool
    pub max_input_bytes: usize,

    /// Max fee limit for a tx to be accepted by mempool
    pub tx_fee_cap: u128,
}
