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
    #[config(default_t = vec!["0x36615Cf349d7F6344891B1e7CA7C72883F5dc049".into()])]
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
    // default address 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
    #[config(default_t = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110".into())]
    pub signing_key: SecretString,
}
