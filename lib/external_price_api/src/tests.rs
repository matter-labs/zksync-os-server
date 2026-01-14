use std::str::FromStr;

use crate::{APIToken, PriceApiClient};
use alloy::primitives::Address;
use chrono::Utc;
use httpmock::MockServer;
use num::rational::Ratio;
use num::{BigInt, Signed, ToPrimitive};

const TIME_TOLERANCE_MS: i64 = 100;
/// Uniswap (UNI)
pub const TEST_TOKEN_ADDRESS: &str = "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984";
/// 1 UNI = 5.47 USD
const TEST_TOKEN_PRICE_USD: f64 = 5.47;
const PRICE_COMPARE_TOLERANCE: f64 = 0.01;

pub(crate) struct SetupResult {
    pub(crate) client: Box<dyn PriceApiClient>,
}

pub(crate) type SetupFn =
    fn(server: &MockServer, address: Address, base_token_price: f64) -> SetupResult;

pub(crate) async fn happy_day_test(setup: SetupFn) {
    let server = MockServer::start();
    let address_str = TEST_TOKEN_ADDRESS;
    let address = Address::from_str(address_str).unwrap();

    // APIs return token price in USD (USD per 1 token)
    let SetupResult { client } = setup(&server, address, TEST_TOKEN_PRICE_USD);
    let api_price = client
        .fetch_ratio(APIToken::ERC20 {
            address,
            decimals: 18,
        })
        .await
        .unwrap();

    // Check that the returned price is approximately equal to the expected price.
    let expected_ratio = Ratio::from_float(TEST_TOKEN_PRICE_USD).unwrap()
        / Ratio::from_integer(BigInt::from(10u64).pow(18u32));
    let got_ratio = {
        let numerator = BigInt::from(api_price.ratio.numer().to_owned());
        let denominator = BigInt::from(api_price.ratio.denom().to_owned());
        Ratio::new(numerator, denominator)
    };
    let diff = (got_ratio - &expected_ratio).abs() / expected_ratio;
    assert!(diff.to_f64().unwrap() < PRICE_COMPARE_TOLERANCE);
    assert!((Utc::now() - api_price.timestamp).num_milliseconds() <= TIME_TOLERANCE_MS);
}

pub(crate) async fn error_test(setup: SetupFn) -> anyhow::Error {
    let server = MockServer::start();
    let address_str = TEST_TOKEN_ADDRESS;
    let address = Address::from_str(address_str).unwrap();

    let SetupResult { client } = setup(&server, address, 1.0);
    let api_price = client
        .fetch_ratio(APIToken::ERC20 {
            address,
            decimals: 18,
        })
        .await;

    assert!(api_price.is_err());
    api_price.unwrap_err()
}
