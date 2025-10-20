use std::cmp::min;

/// Helper type for representing the fees of a `TransactionRequest`
#[derive(Debug)]
pub struct CallFees {
    /// EIP-1559 priority fee
    pub max_priority_fee_per_gas: Option<u128>,
    /// Unified gas price setting
    ///
    /// `gasPrice` for legacy,
    /// `maxFeePerGas` for EIP-1559
    pub gas_price: u128,
}

impl CallFees {
    // todo(EIP-4844): handle blob fees
    pub fn ensure_fees(
        call_gas_price: Option<u128>,
        call_max_fee_per_gas: Option<u128>,
        call_max_priority_fee_per_gas: Option<u128>,
        block_base_fee: u128,
        for_estimate_gas: bool,
    ) -> Result<Self, CallFeesError> {
        match (
            call_gas_price,
            call_max_fee_per_gas,
            call_max_priority_fee_per_gas,
        ) {
            (gas_price, None, None) => {
                let gas_price = match (for_estimate_gas, gas_price) {
                    (false, _) => {
                        // either legacy transaction or no fee fields are specified
                        // when no fields are specified, set gas price to zero
                        gas_price.unwrap_or_default()
                    }
                    // only enforce the fee cap if provided input is not zero
                    (_, None | Some(0)) => block_base_fee,
                    (_, Some(gas_price)) if gas_price < block_base_fee => {
                        return Err(CallFeesError::FeeCapTooLow);
                    }
                    (_, Some(gas_price)) => gas_price,
                };
                Ok(Self {
                    gas_price,
                    max_priority_fee_per_gas: None,
                })
            }
            (None, max_fee_per_gas, max_priority_fee_per_gas) => {
                let effective_gas_price = match max_fee_per_gas {
                    Some(max_fee_per_gas) => {
                        let max_priority_fee_per_gas = max_priority_fee_per_gas.unwrap_or_default();

                        // only enforce the fee cap if provided input is not zero
                        if !(max_fee_per_gas == 0 && max_priority_fee_per_gas == 0)
                            && max_fee_per_gas < block_base_fee
                        {
                            return Err(CallFeesError::FeeCapTooLow);
                        }
                        if max_fee_per_gas < max_priority_fee_per_gas {
                            return Err(CallFeesError::TipAboveFeeCap);
                        }
                        min(
                            max_fee_per_gas,
                            block_base_fee
                                .checked_add(max_priority_fee_per_gas)
                                .ok_or(CallFeesError::TipVeryHigh)?,
                        )
                    }
                    None => block_base_fee
                        .checked_add(max_priority_fee_per_gas.unwrap_or_default())
                        .ok_or(CallFeesError::TipVeryHigh)?,
                };

                Ok(Self {
                    gas_price: effective_gas_price,
                    max_priority_fee_per_gas,
                })
            }
            _ => Err(CallFeesError::ConflictingFeeFieldsInRequest),
        }
    }
}

/// Error coming from decoding and validating transaction request fees.
#[derive(Debug, thiserror::Error)]
pub enum CallFeesError {
    /// Thrown when a call contains conflicting fields (legacy, EIP-1559).
    #[error("both `gasPrice` and (`maxFeePerGas` or `maxPriorityFeePerGas`) specified")]
    ConflictingFeeFieldsInRequest,
    /// Thrown if the transaction's fee is less than the base fee of the block.
    #[error("`maxFeePerGas` less than `block.baseFee`")]
    FeeCapTooLow,
    /// Thrown to ensure no one is able to specify a transaction with a tip higher than the total
    /// fee cap.
    #[error("`maxPriorityFeePerGas` higher than `maxFeePerGas`")]
    TipAboveFeeCap,
    /// A sanity error to avoid huge numbers specified in the tip field.
    #[error("`maxPriorityFeePerGas` is too high")]
    TipVeryHigh,
}
