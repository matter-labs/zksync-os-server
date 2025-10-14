use std::time::Duration;

use smart_config::{DescribeConfig, DeserializeConfig, config, value::SecretString};

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatchVerificationConfig {
    /// [server] If we are collecting batch verification signatures
    /// [en] If we are signing batches
    #[config(default_t = false)]
    pub enabled: bool,
    /// [server] Batch verification server address to listen on.
    /// [en] Batch verification server address to connect to.
    #[config(default_t = "0.0.0.0:3072".into())]
    pub address: String,
    /// [server] Threshold (number of needed signatures)
    #[config(default_t = 1)]
    pub threshold: usize,
    /// [server] Accepted signer pubkeys
    #[config(default)]
    pub accepted_signers: Vec<String>,
    /// [server] Iteration timeout
    #[config(default_t = Duration::from_secs(5))]
    pub request_timeout: Duration,
    /// [server] Retry delay between attempts
    #[config(default_t = Duration::from_secs(1))]
    pub retry_delay: Duration,
    /// [server] Total timeout
    #[config(default_t = Duration::from_secs(300))]
    pub total_timeout: Duration,
    /// [EN] Signing key
    #[config(default_t = "0x".into())]
    pub signing_key: SecretString,
}
