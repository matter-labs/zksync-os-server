use alloy::providers::DynProvider;
use zksync_os_types::fee::{FeePerGas, Gas, GasPerPubdata, Pubdata};

/// The amount of gas we need to pay for each non-zero pubdata byte.
/// Note that it is bigger than 16 to account for potential overhead
const L1_GAS_PER_CALLDATA_PUBDATA_BYTE: GasPerPubdata = GasPerPubdata(17);

/// The constant amount of L1 gas that is used as the overhead for the batch. This includes commitment,
/// proof submission and execution.
///
/// Current value was picked based on empirical data on stage:
/// * Commit 136_131 gas - https://sepolia.etherscan.io/tx/0x46bb5074df6201dca4f06e7d02e1f16bd1ad1449d226b83b884f6f9bc570061c
/// * Prove 72_417 gas - https://sepolia.etherscan.io/tx/0xb311f31d9e093916fd03389b9e97f38a1642d90e269fb4b97973edc5acd5a584
/// * Execute 96_116 gas - https://sepolia.etherscan.io/tx/0x44a2f3969df78cdc20b878d143f8b696b05c17d65b822567864f4774c9768711
/// Total is 304_664, but we pick 350_000 to provide safe margin
const L1_BATCH_OVERHEAD: Gas = Gas(350_000);

#[derive(Debug, Clone)]
pub struct BlockFee {
    pub fee_per_gas: FeePerGas,
    pub gas_per_pubdata: GasPerPubdata,
}

pub struct FeeEstimator {
    max_pubdata_per_batch: Pubdata,
    _l1_provider: DynProvider,
}

impl FeeEstimator {
    pub fn new(max_pubdata_per_batch: Pubdata, l1_provider: DynProvider) -> Self {
        Self {
            max_pubdata_per_batch,
            _l1_provider: l1_provider,
        }
    }

    pub async fn estimate(&self) -> anyhow::Result<BlockFee> {
        // todo: block below is what the real implementation could look like, it is disabled for now
        //       as dynamic fee per gas involves a lot of changes in the code.

        // // Estimates the EIP1559 `maxFeePerGas` and `maxPriorityFeePerGas` for L1. This uses default
        // // estimator based on the work by [MetaMask](https://github.com/MetaMask/core/blob/0fd4b397e7237f104d1c81579a0c4321624d076b/packages/gas-fee-controller/src/fetchGasEstimatesViaEthFeeHistory/calculateGasFeeEstimatesForPriorityLevels.ts#L56);
        // // constants for "medium" priority level are used.
        // let l1_eip1559_fees = self.l1_provider.estimate_eip1559_fees().await?;
        // let l1_fee_per_gas =
        //     FeePerGas(l1_eip1559_fees.max_fee_per_gas + l1_eip1559_fees.max_priority_fee_per_gas);
        // let l1_batch_overhead_per_gas = (L1_BATCH_OVERHEAD * l1_fee_per_gas) / MAX_GAS_PER_BATCH;
        // let fee_per_gas = MINIMAL_L2_GAS_PRICE + l1_batch_overhead_per_gas;

        let l1_gas_per_pubdata = L1_GAS_PER_CALLDATA_PUBDATA_BYTE;
        let l1_batch_overhead_per_pubdata = L1_BATCH_OVERHEAD / self.max_pubdata_per_batch;
        let gas_per_pubdata = l1_gas_per_pubdata + l1_batch_overhead_per_pubdata;

        // fixme: temporary workaround while we use gasPerPubdata=1 for all Ethereum transactions
        let gas_per_pubdata = gas_per_pubdata.min(GasPerPubdata(1));

        Ok(BlockFee {
            fee_per_gas: FeePerGas(1000),
            gas_per_pubdata,
        })
    }
}
