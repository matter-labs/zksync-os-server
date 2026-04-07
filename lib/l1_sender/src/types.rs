/// EIP-1559 gas parameters for a submitted transaction.
#[derive(Clone, Debug)]
pub struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// EIP-4844 blob base fee, included even for non-blob transactions so that
    /// `estimate_gas_params` can be a single call site for all fee estimation.
    pub fee_per_blob_gas: u128,
}

impl GasParams {
    /// Minimum fee increase required by miners to accept a replacement transaction
    /// over the one already in the mempool, expressed as a percentage multiplier.
    /// EIP-1559 / geth require a 10% bump on both fee fields.
    const REPLACEMENT_BUMP: u128 = 110;

    /// Returns a new `GasParams` with each field clamped to the corresponding cap.
    pub fn clamped_to(&self, caps: &GasParams) -> GasParams {
        GasParams {
            max_fee_per_gas: self.max_fee_per_gas.min(caps.max_fee_per_gas),
            max_priority_fee_per_gas: self
                .max_priority_fee_per_gas
                .min(caps.max_priority_fee_per_gas),
            fee_per_blob_gas: self.fee_per_blob_gas.min(caps.fee_per_blob_gas),
        }
    }

    /// Returns a new `GasParams` that is guaranteed to satisfy the EIP-1559 10% replacement bump
    /// rule,
    pub fn with_minimum_replacement_bump(&self, previous: &GasParams) -> GasParams {
        let min_fee = (previous.max_fee_per_gas * Self::REPLACEMENT_BUMP).div_ceil(100);
        let min_priority =
            (previous.max_priority_fee_per_gas * Self::REPLACEMENT_BUMP).div_ceil(100);
        let min_blob = (previous.fee_per_blob_gas * Self::REPLACEMENT_BUMP).div_ceil(100);
        GasParams {
            max_fee_per_gas: self.max_fee_per_gas.max(min_fee),
            max_priority_fee_per_gas: self.max_priority_fee_per_gas.max(min_priority),
            fee_per_blob_gas: self.fee_per_blob_gas.max(min_blob),
        }
    }
}
