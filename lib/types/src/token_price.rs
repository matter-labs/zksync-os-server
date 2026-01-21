use chrono::{DateTime, Utc};
use num::rational::Ratio;
use num::{BigInt, BigUint};

/// Struct to represent API response containing ratio and timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenApiRatio {
    /// Ratio representing the USD price of the token unit.
    pub ratio: Ratio<BigUint>,
    /// Either the timestamp of the quote or the timestamp of the request.
    pub timestamp: DateTime<Utc>,
}

impl TokenApiRatio {
    pub fn from_f64_decimals_and_timestamp(
        value: f64,
        decimals: u8,
        timestamp: Option<DateTime<Utc>>,
    ) -> Self {
        assert!(value > 0.0, "Value must be positive");
        let signed_ratio =
            Ratio::<BigInt>::from_float(value).expect("Failed to convert float to ratio");

        let mut ratio = Ratio::<BigUint>::new(
            signed_ratio.numer().to_biguint().unwrap(),
            signed_ratio.denom().to_biguint().unwrap(),
        );
        ratio /= BigUint::from(10u64).pow(decimals as u32);

        Self {
            ratio,
            timestamp: timestamp.unwrap_or_else(Utc::now),
        }
    }

    pub fn reciprocal(&self) -> Self {
        Self {
            ratio: self.ratio.recip(),
            timestamp: self.timestamp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenPricesForFees {
    pub base_token_usd_price: TokenApiRatio,
    pub sl_token_usd_price: TokenApiRatio,
}
