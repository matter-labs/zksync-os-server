pub mod cmc_api;
pub mod coingecko_api;
pub mod forced_price_client;
#[cfg(test)]
mod tests;

use alloy::primitives::{Address, address};
use async_trait::async_trait;
use secrecy::SecretString;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;
use zksync_os_types::TokenApiRatio;

/// ZK token address on Ethereum Mainnet
pub const ZK_L1_ADDRESS: Address = address!("0x66a5cfb2e9c529f14fe6364ad1075df3a649c0a5");
pub const ETH_DECIMALS: u8 = 18;
pub const ZK_DECIMALS: u8 = 18;

/// Enum representing the token for which the ratio is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum APIToken {
    ETH, // ETH token
    ERC20 {
        // on Ethereum
        address: Address,
        decimals: u8,
    },
    ZK, // ZK is not tracked by API sources by any Ethereum address (only by L2 address)
}

impl APIToken {
    pub fn from_address_and_decimals(address: Address, decimals: u8) -> Self {
        if address == Address::ZERO || address == Address::with_last_byte(0x01) {
            assert_eq!(decimals, ETH_DECIMALS);
            Self::ETH
        } else if address == ZK_L1_ADDRESS {
            // ZK needs special handling due to being not recognized by API clients
            assert_eq!(decimals, ZK_DECIMALS);
            Self::ZK
        } else {
            Self::ERC20 { address, decimals }
        }
    }

    pub fn decimals(&self) -> u8 {
        match self {
            APIToken::ETH => ETH_DECIMALS,
            APIToken::ERC20 { decimals, .. } => *decimals,
            APIToken::ZK => ZK_DECIMALS,
        }
    }
}

impl fmt::Display for APIToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            APIToken::ETH => write!(f, "ETH"),
            APIToken::ERC20 { address, .. } => write!(f, "ERC20({address})"),
            APIToken::ZK => write!(f, "ZK"),
        }
    }
}

/// Trait that defines the interface for a client connecting with an external API to get prices.
#[async_trait]
pub trait PriceApiClient: Sync + Send + fmt::Debug + 'static {
    /// Returns the "USD<->token base unit" ratio for the input token address.
    /// Base unit is the smallest indivisible unit of the token (wei for ETH, 10^(-decimals) of the token for ERC20).
    /// The returned value is rational number X such that 1 base units = X USD.
    /// Example if 1 token base unit = 0.002 USD, then ratio is 1/500 (1 base unit = 1/500 USD)
    async fn fetch_ratio(&self, token: APIToken) -> anyhow::Result<TokenApiRatio>;
}

/// Config to force configured token prices in USD.
/// E.g. if needed to force 1 TOKEN = 0.3 USD, that would be represented in a config with price=0.3 for this token.
/// Important: price is **token** price (e.g. for USDC it would be 1), not base token unit price.
#[derive(Debug, Clone)]
pub struct ForcedPriceClientConfig {
    /// Map of token addresses to their forced price in USD for 1 token (not base token unit!).
    pub prices: HashMap<Address, f64>,
    /// Forced fluctuation. It defines how much percent the ratio should fluctuate from its forced
    /// value. If it's 0, then the ForcedPriceClient will return the same quote every time
    /// it's called. Otherwise, ForcedPriceClient will return quote with numerator +/- fluctuation %.
    pub fluctuation: f64,
    /// In order to smooth out fluctuation, consecutive values returned by forced client will not
    /// differ more than next_value_fluctuation percent.
    pub next_value_fluctuation: f64,
}

#[derive(Debug, Clone)]
pub enum ExternalPriceApiClientConfig {
    Forced {
        /// Config for forced price client.
        forced: ForcedPriceClientConfig,
    },
    CoinGecko {
        /// Base URL of the external price API.
        base_url: Option<String>,
        /// API key for the external price API.
        coingecko_api_key: Option<SecretString>,
        /// Timeout for the external price API client.
        client_timeout: Duration,
    },
    CoinMarketCap {
        /// Base URL of the external price API.
        base_url: Option<String>,
        /// API key for the external price API. Required.
        cmc_api_key: SecretString,
        /// Timeout for the external price API client.
        client_timeout: Duration,
    },
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;

    use super::*;

    #[tokio::test]
    async fn test_base_token_from_config() {
        let eth_address1 = Address::ZERO;
        let eth_address2 = address!("0x0000000000000000000000000000000000000001");
        let custom_address = address!("0x0000000000000000000000000000000000001234");

        // Test ETH address recognition
        assert_matches!(
            APIToken::from_address_and_decimals(eth_address1, 18),
            APIToken::ETH
        );
        assert_matches!(
            APIToken::from_address_and_decimals(eth_address2, 18),
            APIToken::ETH
        );
        assert_matches!(APIToken::from_address_and_decimals(custom_address, 18), APIToken::ERC20 { address, .. } if address == custom_address);
    }
}
