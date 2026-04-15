use std::time::Duration;

use secrecy::SecretString;

/// Struct matches zksync_os_server::config::BatchVerificationConfig.
/// See there for documentation
#[derive(Clone, Debug)]
pub struct BatchVerificationConfig {
    pub server_enabled: bool,
    pub client_enabled: bool,
    pub threshold: u64,
    pub accepted_signers: Vec<String>,
    pub request_timeout: Duration,
    pub retry_delay: Duration,
    pub total_timeout: Duration,
    pub signing_key: SecretString,
}
