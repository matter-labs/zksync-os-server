use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::signers::Signer;
use alloy::signers::gcp::GcpSigner;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::utils::secret_key_to_address;
use backon::Retryable;
use std::sync::Arc;
use tokio::sync::OnceCell;

mod gcp;
mod retry;

pub use retry::GkmsRetryPolicy;
use retry::RetryingSigner;

/// Configuration for how a signing key is provided.
///
/// For GCP KMS keys, the signer (and its underlying API client) is created lazily
/// on first use and cached for subsequent calls. Cloned configs share the same cache
/// via `Arc`, so multiple calls to [`address`](Self::address) and
/// [`register_with_wallet`](Self::register_with_wallet) only create one GCP client.
#[derive(Debug)]
pub enum SignerConfig {
    /// Use a local private key for signing.
    Local(SigningKey),
    /// Use a Google Cloud KMS key for signing.
    GcpKms {
        /// Full resource name of the KMS key version, e.g.
        /// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
        resource_name: String,
        /// Retry policy for KMS network operations (client creation, public key fetch,
        /// transaction signing). Starts as [`GkmsRetryPolicy::default`]; override via
        /// [`set_gkms_retry_policy`](Self::set_gkms_retry_policy).
        retry_policy: GkmsRetryPolicy,
        /// Lazily-initialized GCP signer, shared across clones.
        cached_signer: Arc<OnceCell<GcpSigner>>,
    },
}

impl Clone for SignerConfig {
    fn clone(&self) -> Self {
        match self {
            Self::Local(sk) => Self::Local(sk.clone()),
            Self::GcpKms {
                resource_name,
                retry_policy,
                cached_signer,
            } => Self::GcpKms {
                resource_name: resource_name.clone(),
                retry_policy: *retry_policy,
                cached_signer: cached_signer.clone(),
            },
        }
    }
}

impl SignerConfig {
    /// Creates a GCP KMS config with an empty signer cache and the default retry policy.
    pub fn gcp_kms(resource_name: String) -> Self {
        Self::GcpKms {
            resource_name,
            retry_policy: GkmsRetryPolicy::default(),
            cached_signer: Arc::new(OnceCell::new()),
        }
    }

    /// Overrides the retry policy for GCP KMS operations. No-op for local keys.
    pub fn set_gkms_retry_policy(&mut self, policy: GkmsRetryPolicy) {
        match self {
            Self::GcpKms { retry_policy, .. } => *retry_policy = policy,
            Self::Local(_) => {}
        }
    }

    /// Returns the cached GCP signer, creating it on first call.
    ///
    /// Creation involves network calls (KMS client setup + public key fetch), so it is
    /// retried per the key's [`GkmsRetryPolicy`].
    async fn get_gcp_signer(&self) -> anyhow::Result<&GcpSigner> {
        match self {
            Self::GcpKms {
                resource_name,
                retry_policy,
                cached_signer,
            } => {
                cached_signer
                    .get_or_try_init(|| async {
                        (|| gcp::create_gcp_signer(resource_name))
                            .retry(retry_policy.exponential())
                            .notify(|err, delay| {
                                tracing::warn!(
                                    %resource_name,
                                    ?delay,
                                    %err,
                                    "failed to create GCP KMS signer; retrying"
                                );
                            })
                            .await
                    })
                    .await
            }
            Self::Local(_) => anyhow::bail!("get_gcp_signer called on Local variant"),
        }
    }

    /// Returns the Ethereum address for this signer.
    ///
    /// For local keys the address is derived locally. For GCP KMS keys a network
    /// call is made on first invocation to fetch the public key; subsequent calls
    /// return the cached address.
    pub async fn address(&self) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => Ok(secret_key_to_address(sk)),
            Self::GcpKms { .. } => {
                let signer = self.get_gcp_signer().await?;
                Ok(signer.address())
            }
        }
    }

    /// Creates the appropriate signer, registers it with the wallet, and returns the Ethereum address.
    ///
    /// For GCP KMS, reuses the cached signer (cloning it for wallet registration). The
    /// registered signer retries failed sign attempts per the key's [`GkmsRetryPolicy`],
    /// since every KMS signing RPC goes over the network and can fail transiently.
    pub async fn register_with_wallet(
        &self,
        wallet: &mut EthereumWallet,
    ) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => {
                let signer = PrivateKeySigner::from_signing_key(sk.clone());
                let address = signer.address();
                wallet.register_signer(signer);
                Ok(address)
            }
            Self::GcpKms {
                resource_name,
                retry_policy,
                ..
            } => {
                let signer = self.get_gcp_signer().await?.clone();
                let address = signer.address();
                tracing::info!(%address, %resource_name, "registered GCP KMS signer");
                wallet.register_signer(RetryingSigner::new(
                    signer,
                    *retry_policy,
                    resource_name.clone(),
                ));
                Ok(address)
            }
        }
    }
}
