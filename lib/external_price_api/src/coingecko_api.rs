use crate::{APIToken, PriceApiClient};
use anyhow::Context;
use async_trait::async_trait;
use num::ToPrimitive;
use reqwest;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use url::Url;
use zksync_os_types::TokenApiRatio;

#[derive(Debug)]
pub struct CoinGeckoPriceAPIClient {
    base_url: Url,
    client: reqwest::Client,
}

const DEFAULT_COINGECKO_API_URL: &str = "https://pro-api.coingecko.com";
const COINGECKO_AUTH_HEADER: &str = "x-cg-pro-api-key";
const USER_AGENT_HEADER: &str = "user-agent";
const USER_AGENT_VALUE: &str =
    "zksync-os-server/0.1 (https://github.com/matter-labs/zksync-os-server)";
const ETH_ID: &str = "ethereum";
const ZKSYNC_ID: &str = "zksync";

impl CoinGeckoPriceAPIClient {
    pub fn new(
        base_url: Option<String>,
        api_key: Option<SecretString>,
        client_timeout: Duration,
    ) -> anyhow::Result<Self> {
        tracing::debug!(
            ?base_url,
            ?client_timeout,
            "Creating CoinGeckoPriceAPIClient"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(USER_AGENT_HEADER),
            HeaderValue::from_static(USER_AGENT_VALUE),
        );
        if let Some(api_key) = api_key {
            let mut value = HeaderValue::from_str(api_key.expose_secret())
                .context("Failed to create header value")?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static(COINGECKO_AUTH_HEADER), value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(client_timeout)
            .build()
            .context("Failed to build reqwest client")?;

        let base_url = base_url.unwrap_or(DEFAULT_COINGECKO_API_URL.to_string());

        Ok(Self {
            base_url: Url::parse(&base_url).context("Failed to parse CoinGecko URL")?,
            client,
        })
    }
}

#[async_trait]
impl PriceApiClient for CoinGeckoPriceAPIClient {
    async fn fetch_ratio(&self, token: APIToken) -> anyhow::Result<TokenApiRatio> {
        let (path, token_id) = match &token {
            APIToken::ETH => (
                format!("/api/v3/simple/price?ids={ETH_ID}&vs_currencies=usd"),
                ETH_ID.to_string(),
            ),
            APIToken::ERC20 { address, .. } => (
                format!(
                    "/api/v3/simple/token_price/ethereum?contract_addresses={address}&vs_currencies=usd"
                ),
                address.to_string().to_lowercase(),
            ),
            APIToken::ZK => (
                format!("/api/v3/simple/price?ids={ZKSYNC_ID}&vs_currencies=usd"),
                ZKSYNC_ID.to_string(),
            ),
        };
        let decimals = token.decimals();
        let price_url = self.base_url.join(&path).expect("failed to join URL path");

        let response = self.client.get(price_url).send().await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Http error while fetching token price. Status: {}, token: {token_id}, msg: {}",
            response.status(),
            response.text().await.unwrap_or(String::new())
        );

        let cg_response = response.json::<CoinGeckoPriceResponse>().await?;
        let price_f64 = cg_response
            .get_price(&token_id, &"usd".to_owned())
            .with_context(|| format!("Price not found for token: {token_id}"))?;

        let res = TokenApiRatio::from_f64_decimals_and_timestamp(price_f64, decimals, None);
        tracing::trace!("fetch_ratio({token:?}): ratio {:?}", res.ratio.to_f64());
        Ok(res)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoinGeckoPriceResponse {
    #[serde(flatten)]
    pub(crate) prices: HashMap<String, HashMap<String, f64>>,
}

impl CoinGeckoPriceResponse {
    fn get_price(&self, address: &str, currency: &String) -> Option<f64> {
        self.prices
            .get(address)
            .and_then(|price| price.get(currency).copied())
    }
}

#[cfg(test)]
mod test {
    use alloy::primitives::Address;
    use httpmock::MockServer;
    use std::time::Duration;

    use super::*;
    use crate::tests::*;

    fn get_mock_response(address: &str, price: f64) -> String {
        format!("{{\"{address}\":{{\"usd\":{price}}}}}")
    }

    #[test]
    fn test_mock_response() {
        // curl "https://api.coingecko.com/api/v3/simple/token_price/ethereum?contract_addresses=0x1f9840a85d5af5bf1d1762f925bdaddc4201f984&vs_currencies=usd"
        // {"0x1f9840a85d5af5bf1d1762f925bdaddc4201f984":{"usd":5.47}}
        assert_eq!(
            get_mock_response("0x1f9840a85d5af5bf1d1762f925bdaddc4201f984", 5.47),
            r#"{"0x1f9840a85d5af5bf1d1762f925bdaddc4201f984":{"usd":5.47}}"#
        )
    }

    fn add_mock_by_address(
        server: &MockServer,
        // use string explicitly to verify that conversion of the address to string works as expected
        address: Address,
        price: Option<f64>,
        api_key: Option<String>,
    ) {
        server.mock(|mut when, then| {
            when = when
                .method(httpmock::Method::GET)
                .path("/api/v3/simple/token_price/ethereum");

            when = when.query_param("contract_addresses", address.to_string());
            when = when.query_param("vs_currencies", "usd");
            api_key.map(|key| when.header(COINGECKO_AUTH_HEADER, key));

            if let Some(p) = price {
                then.status(200)
                    .body(get_mock_response(&address.to_string().to_lowercase(), p));
            } else {
                // requesting with invalid/unknown address results in empty json
                // example:
                // $ curl "https://api.coingecko.com/api/v3/simple/token_price/ethereum?contract_addresses=0x000000000000000000000000000000000000dead&vs_currencies=usd"
                // {}
                then.status(200).body("{}");
            };
        });
    }

    fn happy_day_setup(
        api_key: Option<String>,
        server: &MockServer,
        address: Address,
        base_token_price: f64,
    ) -> SetupResult {
        add_mock_by_address(server, address, Some(base_token_price), api_key.clone());
        SetupResult {
            client: Box::new(
                CoinGeckoPriceAPIClient::new(
                    Some(server.url("")),
                    api_key.map(Into::into),
                    Duration::from_secs(1),
                )
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_happy_day_with_api_key() {
        happy_day_test(
            |server: &MockServer, address: Address, base_token_price: f64| {
                happy_day_setup(
                    Some("test-key".to_string()),
                    server,
                    address,
                    base_token_price,
                )
            },
        )
        .await
    }

    #[tokio::test]
    async fn test_happy_day_with_no_api_key() {
        happy_day_test(
            |server: &MockServer, address: Address, base_token_price: f64| {
                happy_day_setup(None, server, address, base_token_price)
            },
        )
        .await
    }

    fn error_404_setup(
        server: &MockServer,
        _address: Address,
        _base_token_price: f64,
    ) -> SetupResult {
        // just don't add mock
        SetupResult {
            client: Box::new(
                CoinGeckoPriceAPIClient::new(
                    Some(server.url("")),
                    Some("FILLER".to_string().into()),
                    Duration::from_secs(1),
                )
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_error_404() {
        let error_string = error_test(error_404_setup).await.to_string();
        assert!(
            error_string
                .starts_with("Http error while fetching token price. Status: 404 Not Found"),
            "Error was: {}",
            &error_string
        )
    }

    fn error_missing_setup(
        server: &MockServer,
        address: Address,
        _base_token_price: f64,
    ) -> SetupResult {
        let api_key = Some("FILLER".to_string());

        add_mock_by_address(server, address, None, api_key.clone());
        SetupResult {
            client: Box::new(
                CoinGeckoPriceAPIClient::new(
                    Some(server.url("")),
                    api_key.map(Into::into),
                    Duration::from_secs(1),
                )
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_error_missing() {
        let error_string = error_test(error_missing_setup).await.to_string();
        assert!(
            error_string.starts_with("Price not found for token"),
            "Error was: {error_string}",
        )
    }
}
