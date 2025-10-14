use std::time::Duration;

use smart_config::{DescribeConfig, DeserializeConfig, config, value::SecretString};

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatchVerificationConfig {
    /// If we are using batch verification
    #[config(default_t = false)]
    pub enabled: bool,
    /// Batch verification server address to listen on.
    #[config(default_t = "0.0.0.0:3072".into())]
    pub address: String,
    /// Threshold (number of needed signatures)
    #[config(default_t = 1)]
    pub threshold: usize,
    /// Accepted signer pubkeys
    #[config(default)]
    pub accepted_signers: Vec<String>,
    /// Iteration timeout
    #[config(default_t = Duration::from_secs(5))]
    pub request_timeout: Duration,
    /// Retry delay between attempts
    #[config(default_t = Duration::from_secs(1))]
    pub retry_delay: Duration,
    /// Total timeout
    #[config(default_t = Duration::from_secs(300))]
    pub total_timeout: Duration,
    /// Signing key
    #[config(default_t = "0x".into())]
    pub signing_key: SecretString,
}
