use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::signers::Signer;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;

mod gcp;

/// Configuration for how an operator signing key is provided.
#[derive(Clone)]
pub enum OperatorSignerConfig {
    /// Use a local private key for signing.
    Local(SigningKey),
    /// Use a Google Cloud KMS key for signing.
    ///
    /// The signer is initialized on demand via [`address`](Self::address)
    /// or [`register_with_wallet`](Self::register_with_wallet).
    GcpKms {
        /// Full resource name of the KMS key version, e.g.
        /// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
        resource_name: String,
    },
}

impl std::fmt::Debug for OperatorSignerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(_) => f.debug_tuple("Local").field(&"[REDACTED]").finish(),
            Self::GcpKms { resource_name } => f
                .debug_struct("GcpKms")
                .field("resource_name", resource_name)
                .finish(),
        }
    }
}

impl OperatorSignerConfig {
    /// Returns the Ethereum address for this signer.
    ///
    /// For local keys the address is derived locally. For GCP KMS keys a network
    /// call is made to fetch the public key.
    pub async fn address(&self) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => Ok(PrivateKeySigner::from_signing_key(sk.clone()).address()),
            Self::GcpKms { resource_name } => {
                let signer = gcp::create_gcp_signer(resource_name).await?;
                Ok(signer.address())
            }
        }
    }

    /// Creates the appropriate signer, registers it with the wallet, and returns the Ethereum address.
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
            Self::GcpKms { resource_name } => {
                let signer = gcp::create_gcp_signer(resource_name).await?;
                let address = signer.address();
                tracing::info!(%address, %resource_name, "initialized GCP KMS signer");
                wallet.register_signer(signer);
                Ok(address)
            }
        }
    }
}
