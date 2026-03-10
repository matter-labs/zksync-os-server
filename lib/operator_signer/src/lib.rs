use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::signers::Signer;
use alloy::signers::gcp::GcpSigner;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;
use std::sync::Arc;
use tokio::sync::OnceCell;

mod gcp;

/// Configuration for how an operator signing key is provided.
///
/// For GCP KMS keys, the signer (and its underlying API client) is created lazily
/// on first use and cached for subsequent calls. Cloned configs share the same cache
/// via `Arc`, so multiple calls to [`address`](Self::address) and
/// [`register_with_wallet`](Self::register_with_wallet) only create one GCP client.
pub enum OperatorSignerConfig {
    /// Use a local private key for signing.
    Local(SigningKey),
    /// Use a Google Cloud KMS key for signing.
    GcpKms {
        /// Full resource name of the KMS key version, e.g.
        /// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
        resource_name: String,
        /// Lazily-initialized GCP signer, shared across clones.
        cached_signer: Arc<OnceCell<GcpSigner>>,
    },
}

impl Clone for OperatorSignerConfig {
    fn clone(&self) -> Self {
        match self {
            Self::Local(sk) => Self::Local(sk.clone()),
            Self::GcpKms {
                resource_name,
                cached_signer,
            } => Self::GcpKms {
                resource_name: resource_name.clone(),
                cached_signer: cached_signer.clone(),
            },
        }
    }
}

impl std::fmt::Debug for OperatorSignerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(_) => f.debug_tuple("Local").field(&"[REDACTED]").finish(),
            Self::GcpKms { resource_name, .. } => f
                .debug_struct("GcpKms")
                .field("resource_name", resource_name)
                .finish(),
        }
    }
}

impl OperatorSignerConfig {
    /// Creates a GCP KMS config with an empty signer cache.
    pub fn gcp_kms(resource_name: String) -> Self {
        Self::GcpKms {
            resource_name,
            cached_signer: Arc::new(OnceCell::new()),
        }
    }

    /// Returns the cached GCP signer, creating it on first call.
    async fn get_gcp_signer(&self) -> anyhow::Result<&GcpSigner> {
        match self {
            Self::GcpKms {
                resource_name,
                cached_signer,
            } => cached_signer
                .get_or_try_init(|| gcp::create_gcp_signer(resource_name))
                .await,
            Self::Local(_) => unreachable!("get_gcp_signer called on Local variant"),
        }
    }

    /// Returns the Ethereum address for this signer.
    ///
    /// For local keys the address is derived locally. For GCP KMS keys a network
    /// call is made on first invocation to fetch the public key; subsequent calls
    /// return the cached address.
    pub async fn address(&self) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => Ok(PrivateKeySigner::from_signing_key(sk.clone()).address()),
            Self::GcpKms { .. } => {
                let signer = self.get_gcp_signer().await?;
                Ok(signer.address())
            }
        }
    }

    /// Creates the appropriate signer, registers it with the wallet, and returns the Ethereum address.
    ///
    /// For GCP KMS, reuses the cached signer (cloning it for wallet registration).
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
            Self::GcpKms { resource_name, .. } => {
                let signer = self.get_gcp_signer().await?.clone();
                let address = signer.address();
                tracing::info!(%address, %resource_name, "registered GCP KMS signer");
                wallet.register_signer(signer);
                Ok(address)
            }
        }
    }
}
