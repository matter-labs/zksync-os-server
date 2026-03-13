use crate::execution::metrics::EXECUTION_METRICS;
use alloy::eips::eip4844::FIELD_ELEMENTS_PER_BLOB;
use alloy::primitives::U256;
use num::rational::Ratio;
use num::{BigUint, ToPrimitive};
use tokio::sync::watch;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{PubdataMode, TokenPricesForFees};

/// Fee-related configuration.
#[derive(Debug, Clone)]
pub struct FeeConfig {
    /// Price for one unit of native resource in USD.
    /// Default is set based on the current estimate of proving price.
    pub native_price_usd: Ratio<BigUint>,
    /// Override for base fee (in base token units).
    /// If set, base fee will be constant and equal to this value.
    pub base_fee_override: Option<BigUint>,
    /// Defines how many native resource units are equivalent to one gas unit in terms of price.
    pub native_per_gas: u64,
    /// Override for pubdata price (in base token units).
    /// If set, pubdata price will be constant and equal to this value.
    pub pubdata_price_override: Option<BigUint>,
    /// Cap for pubdata price (in base token units). If set, pubdata price will not exceed this value.
    /// Note:
    /// - has no effect if `pubdata_price_override` is set.
    /// - if pubdata cap is reached, chain operator may operate at a loss.
    pub pubdata_price_cap: Option<BigUint>,
    /// Override for native price (in base token units).
    /// If set, native price will be constant and equal to this value.
    pub native_price_override: Option<BigUint>,
}

/// Provider of fee parameters for block execution.
#[derive(Debug)]
pub struct FeeProvider {
    fee_config: FeeConfig,
    previous_block_fee_params: Option<FeeParams>,
    pubdata_price_provider: watch::Receiver<Option<U256>>,
    blob_fill_ratio_provider: watch::Receiver<Option<Ratio<u64>>>,
    token_price_provider: watch::Receiver<Option<TokenPricesForFees>>,
    pubdata_mode: Option<PubdataMode>,
}

impl FeeProvider {
    pub fn new(
        fee_config: FeeConfig,
        previous_block_fee_params: Option<FeeParams>,
        pubdata_price_provider: watch::Receiver<Option<U256>>,
        blob_fill_ratio_provider: watch::Receiver<Option<Ratio<u64>>>,
        token_price_provider: watch::Receiver<Option<TokenPricesForFees>>,
        pubdata_mode: Option<PubdataMode>,
    ) -> Self {
        Self {
            fee_config,
            previous_block_fee_params,
            pubdata_price_provider,
            blob_fill_ratio_provider,
            token_price_provider,
            pubdata_mode,
        }
    }

    pub async fn produce_fee_params(&mut self) -> anyhow::Result<FeeParams> {
        let token_prices = self
            .token_price_provider
            .wait_for(|prices| prices.is_some())
            .await?
            .clone()
            .unwrap();

        let native_price = self.calculate_native_price(&token_prices);
        let eip1559_basefee = self.calculate_base_fee(&native_price);
        let pubdata_price = self.calculate_pubdata_price(&native_price, &token_prices);
        Self::record_metrics(&native_price, &eip1559_basefee, &pubdata_price);

        let native_price = biguint_to_u256_checked(&native_price).unwrap_or_else(|| {
            tracing::warn!(
                "Calculated native_price {native_price} exceeds U256::MAX, capping to U256::MAX"
            );
            U256::MAX
        });
        let eip1559_basefee = biguint_to_u256_checked(&eip1559_basefee)
            .unwrap_or_else(|| {
                tracing::warn!("Calculated eip1559_basefee {eip1559_basefee} exceeds U256::MAX, capping to U256::MAX");
                U256::MAX
            });
        let pubdata_price = biguint_to_u256_checked(&pubdata_price).unwrap_or_else(|| {
            tracing::warn!(
                "Calculated pubdata_price {pubdata_price} exceeds U256::MAX, capping to U256::MAX"
            );
            U256::MAX
        });

        let fee_params = FeeParams {
            eip1559_basefee,
            native_price,
            pubdata_price,
        };
        tracing::debug!(?fee_params, "Produced fee params");

        Ok(fee_params)
    }

    fn calculate_native_price(&self, token_prices: &TokenPricesForFees) -> BigUint {
        if let Some(o) = self.fee_config.native_price_override.clone() {
            return o;
        }

        let desired_native_price_usd = &self.fee_config.native_price_usd;

        // Convert USD price to base token price.
        let desired_native_price = {
            let price = desired_native_price_usd / &token_prices.base_token_usd_price.ratio;
            price.ceil().to_integer()
        };

        // Limit native price change akin to EIP-1559 rule (up to 12.5%).
        let (min_native_price, max_native_price) = match self
            .previous_block_fee_params
            .map(|p| p.native_price)
        {
            Some(previous_native_price) => {
                const NATIVE_PRICE_MAX_CHANGE_DENOMINATOR: u32 = 8;

                let previous_native_price =
                    BigUint::from_bytes_le(previous_native_price.as_le_slice());

                let min_native_price = &previous_native_price
                    - (&previous_native_price / BigUint::from(NATIVE_PRICE_MAX_CHANGE_DENOMINATOR));
                let max_native_price = &previous_native_price
                    + (&previous_native_price / BigUint::from(NATIVE_PRICE_MAX_CHANGE_DENOMINATOR))
                        .max(BigUint::from(1u32));

                (min_native_price, max_native_price)
            }
            None => {
                // No previous price, allow any price.
                (desired_native_price.clone(), desired_native_price.clone())
            }
        };
        let native_price = desired_native_price
            .clone()
            .clamp(min_native_price.clone(), max_native_price.clone());
        tracing::debug!(
            %native_price,
            %desired_native_price,
            %min_native_price,
            %max_native_price,
            "Calculated native price",
        );

        native_price
    }

    fn calculate_base_fee(&self, native_price: &BigUint) -> BigUint {
        if let Some(o) = self.fee_config.base_fee_override.clone() {
            return o;
        }

        // EIP-1559 base fee is proportional to native price.
        // Thus, we do not need it to clamp separately.
        native_price * self.fee_config.native_per_gas
    }

    fn calculate_pubdata_price(
        &self,
        native_price: &BigUint,
        token_prices: &TokenPricesForFees,
    ) -> BigUint {
        if let Some(o) = self.fee_config.pubdata_price_override.clone() {
            return o;
        }

        let base_pubdata_price_in_sl_token = BigUint::from_bytes_le(
            self.pubdata_price_provider
                .borrow()
                .expect("Pubdata price must be available")
                .as_le_slice(),
        );
        let sl_to_base_ratio =
            &token_prices.sl_token_usd_price.ratio / &token_prices.base_token_usd_price.ratio;
        let base_pubdata_price = {
            let price = Ratio::from_integer(base_pubdata_price_in_sl_token) * &sl_to_base_ratio;
            price.ceil().to_integer()
        };

        let desired_pubdata_price = if self
            .pubdata_mode
            .expect("pubdata_mode must be set when producing blocks")
            == PubdataMode::Blobs
        {
            // Blobs are special in a way that
            // 1. They require additional overhead depending on native price.
            // 2. Blob fill ratio affects the effective pubdata price.

            // TODO(698): Import constants from zksync-os when available.
            // Amount of native resource spent per blob.
            const NATIVE_PER_BLOB: u64 = 50_000_000;
            // Effective number of bytes stored in a blob for `SimpleCoder`.
            const BYTES_USED_PER_BLOB: u64 = (FIELD_ELEMENTS_PER_BLOB - 1) * 31;
            // Amount of native resource spent per pubdata byte (assuming blob is fully filled).
            const NATIVE_PER_BLOB_BYTE: u64 = NATIVE_PER_BLOB / BYTES_USED_PER_BLOB;
            // Default blob fill ratio to be used before `blob_fill_ratio_provider` is initialized.
            const DEFAULT_FILL_RATIO: Ratio<u64> = Ratio::new_raw(1, 2);

            let native_overhead = native_price * NATIVE_PER_BLOB_BYTE;
            // Final pubdata price is base price + overhead depending on native price.
            let pubdata_price_with_overhead = &base_pubdata_price + &native_overhead;

            // By default, we assume that blobs are half-filled.
            let fill_ratio =
                (*self.blob_fill_ratio_provider.borrow()).unwrap_or(DEFAULT_FILL_RATIO);
            // Adjust pubdata price according to blob fill ratio.
            // More filled blobs => less pubdata price (since less overhead per byte).
            // pubdata_price := pubdata_price / ratio = pubdata_price * denom / numer
            let pubdata_price = {
                let mut r = Ratio::from_integer(pubdata_price_with_overhead);
                r *= BigUint::from(*fill_ratio.denom());
                r /= BigUint::from(*fill_ratio.numer());
                r.to_integer()
            };

            tracing::debug!(
                desired_pubdata_price = %pubdata_price,
                %base_pubdata_price,
                %native_overhead,
                %fill_ratio,
                "Calculated desired pubdata price for blobs"
            );
            if let Some(r) = fill_ratio.to_f64() {
                EXECUTION_METRICS.blob_fill_ratio.set(r);
            }

            pubdata_price
        } else {
            base_pubdata_price
        };

        // Limit pubdata price increase to 1.5x per block.
        let mut pubdata_price = if let Some(prev_pubdata_price) =
            self.previous_block_fee_params.map(|p| p.pubdata_price)
        {
            let capped_price = {
                const PUBDATA_PRICE_MAX_CHANGE_DENOMINATOR: u32 = 2;

                let mut r = BigUint::from_bytes_le(prev_pubdata_price.as_le_slice());
                r += (&r / BigUint::from(PUBDATA_PRICE_MAX_CHANGE_DENOMINATOR))
                    .max(BigUint::from(1u32));
                r
            };

            if capped_price < desired_pubdata_price {
                tracing::debug!(
                    %capped_price,
                    %prev_pubdata_price,
                    %desired_pubdata_price,
                    "Capping pubdata price to 1.5*prev_pubdata_price",
                );
            }
            desired_pubdata_price.min(capped_price)
        } else {
            desired_pubdata_price
        };

        if let Some(cap) = self.fee_config.pubdata_price_cap.clone()
            && pubdata_price > cap
        {
            tracing::debug!(
                %cap,
                %pubdata_price,
                "Capping pubdata price according to config",
            );
            pubdata_price = cap;
        }

        pubdata_price
    }

    pub fn on_canonical_state_change(&mut self, replay_record: &ReplayRecord) {
        self.previous_block_fee_params = Some(FeeParams {
            eip1559_basefee: replay_record.block_context.eip1559_basefee,
            native_price: replay_record.block_context.native_price,
            pubdata_price: replay_record.block_context.pubdata_price,
        });
    }

    fn record_metrics(native_price: &BigUint, base_fee: &BigUint, pubdata_price: &BigUint) {
        if let Some(n) = native_price.to_u64() {
            EXECUTION_METRICS.native_price.set(n);
        }
        if let Some(b) = base_fee.to_u64() {
            EXECUTION_METRICS.base_fee.set(b);
        }
        if let Some(p) = pubdata_price.to_u64() {
            EXECUTION_METRICS.pubdata_price.set(p);
        }
    }
}

fn biguint_to_u256_checked(value: &BigUint) -> Option<U256> {
    if value >= &BigUint::from(2u32).pow(256u32) {
        return None;
    }
    let bytes = value.to_bytes_le();
    Some(U256::from_le_slice(&bytes))
}

#[derive(Debug, Clone, Copy)]
pub struct FeeParams {
    pub eip1559_basefee: U256,
    pub native_price: U256,
    pub pubdata_price: U256,
}
