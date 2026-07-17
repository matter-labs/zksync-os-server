use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::signers::Signer;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::utils::secret_key_to_address;
use azure::AzureKmsSigner;
use gcp::GcpKmsSigner;
use std::sync::Arc;
use tokio::sync::OnceCell;

mod azure;
mod gcp;

/// Configuration for how a signing key is provided.
///
/// For cloud KMS keys, the signer (and its underlying API client) is created lazily
/// on first use and cached for subsequent calls. Cloned configs share the same cache
/// via `Arc`, so multiple calls to [`address`](Self::address) and
/// [`register_with_wallet`](Self::register_with_wallet) only create one API client.
#[derive(Debug)]
pub enum SignerConfig {
    /// Use a local private key for signing.
    Local(SigningKey),
    /// Use a Google Cloud KMS key for signing.
    GcpKms {
        /// Full resource name of the KMS key version, e.g.
        /// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
        resource_name: String,
        /// Lazily-initialized GCP signer, shared across clones.
        cached_signer: Arc<OnceCell<GcpKmsSigner>>,
    },
    /// Use an Azure Key Vault (or Managed HSM) key for signing.
    AzureKms {
        /// Full key identifier URL pinned to a version, e.g.
        /// `https://{vault}.vault.azure.net/keys/{name}/{version}`
        key_id: String,
        /// Lazily-initialized Azure signer, shared across clones.
        cached_signer: Arc<OnceCell<AzureKmsSigner>>,
    },
}

impl Clone for SignerConfig {
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
            Self::AzureKms {
                key_id,
                cached_signer,
            } => Self::AzureKms {
                key_id: key_id.clone(),
                cached_signer: cached_signer.clone(),
            },
        }
    }
}

impl SignerConfig {
    /// Creates a GCP KMS config with an empty signer cache.
    pub fn gcp_kms(resource_name: String) -> Self {
        Self::GcpKms {
            resource_name,
            cached_signer: Arc::new(OnceCell::new()),
        }
    }

    /// Creates an Azure Key Vault config with an empty signer cache.
    pub fn azure_kms(key_id: String) -> Self {
        Self::AzureKms {
            key_id,
            cached_signer: Arc::new(OnceCell::new()),
        }
    }

    /// Returns the cached GCP signer, creating it on first call.
    async fn get_gcp_signer(&self) -> anyhow::Result<&GcpKmsSigner> {
        match self {
            Self::GcpKms {
                resource_name,
                cached_signer,
            } => {
                cached_signer
                    .get_or_try_init(|| gcp::create_gcp_signer(resource_name))
                    .await
            }
            _ => anyhow::bail!("get_gcp_signer called on non-GCP variant"),
        }
    }

    /// Returns the cached Azure signer, creating it on first call.
    async fn get_azure_signer(&self) -> anyhow::Result<&AzureKmsSigner> {
        match self {
            Self::AzureKms {
                key_id,
                cached_signer,
            } => {
                cached_signer
                    .get_or_try_init(|| azure::create_azure_signer(key_id))
                    .await
            }
            _ => anyhow::bail!("get_azure_signer called on non-Azure variant"),
        }
    }

    /// Returns the Ethereum address for this signer.
    ///
    /// For local keys the address is derived locally. For cloud KMS keys a network
    /// call is made on first invocation to fetch the public key; subsequent calls
    /// return the cached address.
    pub async fn address(&self) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => Ok(secret_key_to_address(sk)),
            Self::GcpKms { .. } => {
                let signer = self.get_gcp_signer().await?;
                Ok(signer.address())
            }
            Self::AzureKms { .. } => {
                let signer = self.get_azure_signer().await?;
                Ok(signer.address())
            }
        }
    }

    /// Creates the appropriate signer, registers it with the wallet, and returns the Ethereum address.
    ///
    /// For cloud KMS keys, reuses the cached signer (cloning it for wallet registration).
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
            Self::AzureKms { key_id, .. } => {
                let signer = self.get_azure_signer().await?.clone();
                let address = signer.address();
                tracing::info!(%address, %key_id, "registered Azure Key Vault signer");
                wallet.register_signer(signer);
                Ok(address)
            }
        }
    }
}
