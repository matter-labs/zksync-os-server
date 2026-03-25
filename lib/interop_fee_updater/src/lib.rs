use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256, address};
use alloy::providers::DynProvider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::Context as _;
use num::BigUint;
use num::rational::Ratio;
use tokio::sync::watch;
use zksync_os_contract_interface::{IGWAssetTracker, IInteropCenter::interopProtocolFeeCall};
use zksync_os_mempool::subpools::interop_fee::InteropFeeSubpool;
use zksync_os_rpc::{EthCallHandler, ReadRpcStorage};
use zksync_os_types::{L2_INTEROP_CENTER_ADDRESS, TokenPricesForFees};

#[derive(Debug, Clone)]
pub struct InteropFeeUpdaterConfig {
    pub polling_interval: std::time::Duration,
    pub update_deviation_percentage: u32,
}

const GW_ASSET_TRACKER_ADDRESS: Address = address!("0x0000000000000000000000000000000000010010");

pub struct InteropFeeUpdater<RpcStorage> {
    eth_call_handler: EthCallHandler<RpcStorage>,
    gateway_provider: DynProvider,
    interop_fee_subpool: InteropFeeSubpool,
    token_price_receiver: watch::Receiver<Option<TokenPricesForFees>>,
    config: InteropFeeUpdaterConfig,
    last_enqueued_fee: Option<U256>,
}

impl<RpcStorage: ReadRpcStorage> InteropFeeUpdater<RpcStorage> {
    pub fn new(
        eth_call_handler: EthCallHandler<RpcStorage>,
        gateway_provider: DynProvider,
        interop_fee_subpool: InteropFeeSubpool,
        token_price_receiver: watch::Receiver<Option<TokenPricesForFees>>,
        config: InteropFeeUpdaterConfig,
    ) -> Self {
        Self {
            eth_call_handler,
            gateway_provider,
            interop_fee_subpool,
            token_price_receiver,
            config,
            last_enqueued_fee: None,
        }
    }

    pub async fn run(mut self) {
        let mut timer = tokio::time::interval(self.config.polling_interval);

        loop {
            timer.tick().await;
            if let Err(err) = self.loop_iteration().await {
                tracing::warn!("Error in the `interop_fee_updater` loop iteration: {err:#}");
            }
        }
    }

    async fn loop_iteration(&mut self) -> anyhow::Result<()> {
        let Some(token_prices) = self.token_price_receiver.borrow().clone() else {
            tracing::debug!("Token prices are not initialized yet, skipping interop fee update");
            return Ok(());
        };

        let current_interop_fee = self.current_interop_fee()?;
        if let Some(last_enqueued_fee) = self.last_enqueued_fee {
            if current_interop_fee == last_enqueued_fee {
                tracing::debug!(%current_interop_fee, "observed previously enqueued interop fee on-chain");
                self.last_enqueued_fee = None;
            } else {
                tracing::debug!(
                    current_interop_fee = %current_interop_fee,
                    last_enqueued_fee = %last_enqueued_fee,
                    "interop fee update is still pending, skipping",
                );
                return Ok(());
            }
        }

        let gateway_settlement_fee = self.gateway_settlement_fee().await?;
        let target_interop_fee =
            calculate_target_interop_fee(gateway_settlement_fee, &token_prices)
                .context("failed to calculate target interop fee")?;

        if !should_update_fee(
            current_interop_fee,
            target_interop_fee,
            self.config.update_deviation_percentage,
        ) {
            tracing::debug!(
                %current_interop_fee,
                %target_interop_fee,
                deviation_percentage = self.config.update_deviation_percentage,
                "interop fee deviation is within threshold",
            );
            return Ok(());
        }

        tracing::info!(
            %current_interop_fee,
            %target_interop_fee,
            %gateway_settlement_fee,
            "enqueueing interop fee system transaction",
        );
        self.interop_fee_subpool.insert(target_interop_fee).await;
        self.last_enqueued_fee = Some(target_interop_fee);

        Ok(())
    }

    fn current_interop_fee(&self) -> anyhow::Result<U256> {
        let request = TransactionRequest::default()
            .with_to(L2_INTEROP_CENTER_ADDRESS)
            .with_input(Bytes::from(interopProtocolFeeCall {}.abi_encode()));
        let output = self
            .eth_call_handler
            .call_impl(request, Some(BlockId::latest()), None, None)
            .context("failed to call `interopProtocolFee()` on local chain")?;
        let output: [u8; 32] = output
            .as_ref()
            .try_into()
            .context("unexpected `interopProtocolFee()` return data length")?;
        Ok(U256::from_be_bytes(output))
    }

    async fn gateway_settlement_fee(&self) -> anyhow::Result<U256> {
        IGWAssetTracker::new(GW_ASSET_TRACKER_ADDRESS, self.gateway_provider.clone())
            .gatewaySettlementFee()
            .call()
            .await
            .context("failed to call `gatewaySettlementFee()` on gateway chain")
    }
}

fn calculate_target_interop_fee(
    gateway_settlement_fee: U256,
    token_prices: &TokenPricesForFees,
) -> anyhow::Result<U256> {
    let settlement_fee = u256_to_biguint(gateway_settlement_fee);
    let sl_to_base_ratio =
        &token_prices.sl_token_usd_price.ratio / &token_prices.base_token_usd_price.ratio;
    let target_fee = (Ratio::from_integer(settlement_fee) * sl_to_base_ratio)
        .ceil()
        .to_integer();
    biguint_to_u256(target_fee)
}

fn should_update_fee(current_fee: U256, target_fee: U256, deviation_percentage: u32) -> bool {
    if current_fee == target_fee {
        return false;
    }
    if current_fee.is_zero() {
        return true;
    }

    let current_fee = Ratio::from_integer(u256_to_biguint(current_fee));
    let target_fee = Ratio::from_integer(u256_to_biguint(target_fee));
    let diff = if target_fee > current_fee {
        &target_fee - &current_fee
    } else {
        &current_fee - &target_fee
    };
    let deviation = (diff / current_fee) * BigUint::from(100u32);
    deviation >= Ratio::from_integer(BigUint::from(deviation_percentage))
}

fn u256_to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

fn biguint_to_u256(value: BigUint) -> anyhow::Result<U256> {
    let bytes = value.to_bytes_be();
    anyhow::ensure!(bytes.len() <= 32, "value does not fit into U256");

    let mut padded = [0_u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(U256::from_be_bytes(padded))
}

#[cfg(test)]
mod tests {
    use super::{calculate_target_interop_fee, should_update_fee};
    use alloy::primitives::U256;
    use zksync_os_types::{TokenApiRatio, TokenPricesForFees};

    fn token_prices(base_num: u32, base_den: u32, sl_num: u32, sl_den: u32) -> TokenPricesForFees {
        TokenPricesForFees {
            base_token_usd_price: TokenApiRatio::from_f64_decimals_and_timestamp(
                base_num as f64 / base_den as f64,
                0,
                None,
            ),
            sl_token_usd_price: TokenApiRatio::from_f64_decimals_and_timestamp(
                sl_num as f64 / sl_den as f64,
                0,
                None,
            ),
        }
    }

    #[test]
    fn target_fee_uses_sl_to_base_ratio() {
        let prices = token_prices(2, 1, 3, 1);
        let target_fee = calculate_target_interop_fee(U256::from(10), &prices).unwrap();
        assert_eq!(target_fee, U256::from(15));
    }

    #[test]
    fn zero_current_fee_forces_update() {
        assert!(should_update_fee(U256::ZERO, U256::from(1), 10));
    }

    #[test]
    fn deviation_threshold_matches_percent_logic() {
        assert!(!should_update_fee(U256::from(100), U256::from(109), 10));
        assert!(should_update_fee(U256::from(100), U256::from(110), 10));
    }
}
