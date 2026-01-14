use alloy::primitives::Address;
use async_trait::async_trait;
use num::{BigInt, BigUint, ToPrimitive, rational::Ratio};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Mutex;
use zksync_os_types::TokenApiRatio;

use crate::{APIToken, ForcedPriceClientConfig, PriceApiClient, ZK_L1_ADDRESS};

/// Struct for a forced price "client"
/// (price is always a configured "forced" price with a configured fluctuation).
#[derive(Debug)]
pub struct ForcedPriceClient {
    // Configured base ratios for tokens. Note, these are ratios for 1 **token**, not base unit.
    eth_base_ratio: Option<Ratio<BigUint>>,
    zk_base_ratio: Option<Ratio<BigUint>>,
    erc20_base_ratios: HashMap<Address, Ratio<BigUint>>,

    // Previous ratios to apply fluctuation against.
    previous_ratios: Mutex<HashMap<APIToken, Ratio<BigUint>>>,
    // Fluctuation limits in percent.
    total_fluctuation_limit: f64,
    // Fluctuation limit for the next value in percent.
    next_value_fluctuation_limit: f64,
}

impl ForcedPriceClient {
    pub fn new(mut forced_config: ForcedPriceClientConfig) -> Self {
        tracing::debug!(?forced_config, "Creating ForcedPriceClient");

        let eth_base_ratio = forced_config
            .prices
            .remove(&Address::ZERO)
            .or_else(|| forced_config.prices.remove(&Address::with_last_byte(0x01)))
            .map(|p| TokenApiRatio::from_f64_decimals_and_timestamp(p, 0, None).ratio);
        let zk_base_ratio = forced_config
            .prices
            .remove(&ZK_L1_ADDRESS)
            .map(|p| TokenApiRatio::from_f64_decimals_and_timestamp(p, 0, None).ratio);
        let erc20_base_ratios = forced_config
            .prices
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    TokenApiRatio::from_f64_decimals_and_timestamp(v, 0, None).ratio,
                )
            })
            .collect();

        let total_fluctuation_limit = forced_config.fluctuation.clamp(0.0, 100.0);
        let next_value_fluctuation_limit = forced_config.next_value_fluctuation.clamp(0.0, 100.0);

        Self {
            eth_base_ratio,
            zk_base_ratio,
            erc20_base_ratios,
            previous_ratios: Mutex::new(HashMap::new()),
            total_fluctuation_limit,
            next_value_fluctuation_limit,
        }
    }
}

#[async_trait]
impl PriceApiClient for ForcedPriceClient {
    /// Returns the configured ratio with fluctuation applied if enabled
    async fn fetch_ratio(&self, token: APIToken) -> anyhow::Result<TokenApiRatio> {
        let base_ratio = match &token {
            APIToken::ETH => self.eth_base_ratio.clone(),
            APIToken::ZK => self.zk_base_ratio.clone(),
            APIToken::ERC20 { address, .. } => self.erc20_base_ratios.get(address).cloned(),
        };
        let decimals = token.decimals();
        let Some(base_ratio) = base_ratio else {
            anyhow::bail!("No forced price configured for token: {:?}", token);
        };
        let mut previous_ratios = self
            .previous_ratios
            .lock()
            .expect("Failed to lock `previous_ratios` mutex");
        let previous_ratio = previous_ratios
            .get(&token)
            .cloned()
            .unwrap_or(base_ratio.clone());

        let mut rng = rand::rng();
        let next_fluctuation =
            rng.random_range(-self.next_value_fluctuation_limit..self.next_value_fluctuation_limit);
        let multiplier = fluctuation_to_multiplier(next_fluctuation);
        let mut new_ratio = previous_ratio * multiplier;

        let max_ratio = &base_ratio * fluctuation_to_multiplier(self.total_fluctuation_limit);
        let min_ratio = base_ratio * fluctuation_to_multiplier(-self.total_fluctuation_limit);

        new_ratio = new_ratio.clamp(min_ratio, max_ratio);
        previous_ratios.insert(token, new_ratio.clone());

        // Adjust for decimals.
        new_ratio /= BigUint::from(10u64).pow(decimals as u32);

        tracing::trace!("fetch_ratio({token:?}): ratio {:?}", new_ratio.to_f64());
        Ok(TokenApiRatio {
            ratio: new_ratio,
            timestamp: chrono::Utc::now(),
        })
    }
}

fn fluctuation_to_multiplier(fluctuation_percent: f64) -> Ratio<BigUint> {
    let r = Ratio::<BigInt>::from_float(1.0 + (fluctuation_percent / 100.0))
        .expect("Failed to convert `fluctuation_percent` to ratio");
    Ratio::<BigUint>::new(
        r.numer().to_biguint().unwrap(),
        r.denom().to_biguint().unwrap(),
    )
}
