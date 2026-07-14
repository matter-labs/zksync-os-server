//! age recipient/identity backed by a GCP KMS asymmetric key.
//!
//! Encryption wraps the per-file age file key with the RSA-OAEP-SHA256 public key of a KMS
//! `ASYMMETRIC_DECRYPT` key version and is fully local: the node only needs the public key,
//! fetched once at startup. Decryption unwraps the file key with one KMS `AsymmetricDecrypt`
//! call per object; the private key never leaves KMS.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use age_core::format::{FILE_KEY_BYTES, FileKey, Stanza};
use age_core::secrecy::ExposeSecret as _;
use anyhow::Context as _;
use google_cloud_gax::options::RequestOptionsBuilder as _;
use google_cloud_kms_v1::client::KeyManagementService;
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Oaep, RsaPublicKey};
use sha2::Sha256;

/// age header stanza tag for file keys wrapped with a GCP KMS RSA-OAEP key.
const GCP_KMS_STANZA_TAG: &str = "gcp-kms-rsa-oaep";

/// Authentication mode for GCP KMS access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GcpKmsAuthMode {
    /// Ambient authentication (works if the binary runs on Google Cloud, e.g. via workload
    /// identity). This is the primary mode this backend is built for.
    Authenticated,
    /// Authentication via a credentials file at the specified path.
    AuthenticatedWithCredentialFile(PathBuf),
}

/// GCP KMS key configuration for replay archive encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpKmsConfig {
    /// Full key version resource name:
    /// `projects/../locations/../keyRings/../cryptoKeys/../cryptoKeyVersions/..`.
    /// The key must have purpose `ASYMMETRIC_DECRYPT` and an `RSA_DECRYPT_OAEP_*_SHA256`
    /// algorithm.
    pub key_version: String,
    pub auth_mode: GcpKmsAuthMode,
}

/// GCP KMS client bound to one key version, covering the two methods the replay archive needs.
///
/// The underlying client retries transient failures internally, including failures to obtain
/// or refresh access tokens.
#[derive(Clone, Debug)]
pub struct GcpKmsClient {
    client: KeyManagementService,
    key_version: String,
}

impl GcpKmsClient {
    pub async fn new(config: &GcpKmsConfig) -> anyhow::Result<Self> {
        let credentials = match &config.auth_mode {
            GcpKmsAuthMode::Authenticated => crate::gcp::ambient_credentials()?,
            GcpKmsAuthMode::AuthenticatedWithCredentialFile(path) => {
                crate::gcp::credentials_from_file(path)?
            }
        };
        let client = KeyManagementService::builder()
            .with_credentials(credentials)
            .build()
            .await
            .context("failed to create GCP KMS client")?;
        Ok(Self {
            client,
            key_version: config.key_version.clone(),
        })
    }

    pub fn key_version(&self) -> &str {
        &self.key_version
    }

    /// Fetches the public key of the key version, returning `(pem, algorithm)`.
    pub async fn get_public_key(&self) -> anyhow::Result<(String, String)> {
        let response = self
            .client
            .get_public_key()
            .set_name(&self.key_version)
            .send()
            .await
            .with_context(|| {
                format!("failed to fetch GCP KMS public key of {}", self.key_version)
            })?;
        let algorithm = response
            .algorithm
            .name()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{:?}", response.algorithm));
        Ok((response.pem, algorithm))
    }

    /// Decrypts data encrypted with the public key of the key version.
    pub async fn asymmetric_decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let response = self
            .client
            .asymmetric_decrypt()
            .set_name(&self.key_version)
            .set_ciphertext(ciphertext.to_vec())
            // `AsymmetricDecrypt` is side-effect-free, so it is safe to retry even though it is
            // a POST; without this the client only retries failures that happen before the
            // request is sent.
            .with_idempotency(true)
            .send()
            .await
            .with_context(|| {
                format!(
                    "GCP KMS asymmetric decrypt with {} failed",
                    self.key_version
                )
            })?;
        Ok(response.plaintext.to_vec())
    }
}

/// age recipient that wraps file keys with the RSA-OAEP-SHA256 public key of a GCP KMS
/// `ASYMMETRIC_DECRYPT` key version. Wrapping is fully local.
#[derive(Debug, Clone)]
pub struct GcpKmsRecipient {
    public_key: RsaPublicKey,
    key_version: String,
}

impl GcpKmsRecipient {
    pub fn from_public_key_pem(
        pem: &str,
        algorithm: &str,
        key_version: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            algorithm.starts_with("RSA_DECRYPT_OAEP_") && algorithm.ends_with("_SHA256"),
            "unsupported GCP KMS key algorithm {algorithm}; \
             the key version must use an RSA_DECRYPT_OAEP_*_SHA256 algorithm"
        );
        let public_key = RsaPublicKey::from_public_key_pem(pem)
            .context("failed to parse GCP KMS public key PEM")?;
        Ok(Self {
            public_key,
            key_version,
        })
    }

    /// Fetches the public key of the client's key version from KMS.
    pub async fn fetch(client: &GcpKmsClient) -> anyhow::Result<Self> {
        let (pem, algorithm) = client.get_public_key().await?;
        Self::from_public_key_pem(&pem, &algorithm, client.key_version().to_owned())
    }
}

impl age::Recipient for GcpKmsRecipient {
    fn wrap_file_key(
        &self,
        file_key: &FileKey,
    ) -> Result<(Vec<Stanza>, HashSet<String>), age::EncryptError> {
        let body = self
            .public_key
            .encrypt(
                &mut rand_core06::OsRng,
                Oaep::new::<Sha256>(),
                file_key.expose_secret(),
            )
            .map_err(|err| {
                age::EncryptError::Io(io::Error::other(format!(
                    "RSA-OAEP wrapping of age file key failed: {err}"
                )))
            })?;
        Ok((
            vec![Stanza {
                tag: GCP_KMS_STANZA_TAG.to_owned(),
                args: vec![self.key_version.clone()],
                body,
            }],
            HashSet::new(),
        ))
    }
}

/// age identity that unwraps file keys via GCP KMS `AsymmetricDecrypt`, one call per age file.
#[derive(Clone, Debug)]
pub struct GcpKmsIdentity {
    client: GcpKmsClient,
    runtime: tokio::runtime::Handle,
}

impl GcpKmsIdentity {
    /// Must be called inside a tokio runtime. Unwrapping a stanza blocks on the KMS call, so
    /// decryption must run on a blocking thread (e.g. via `spawn_blocking`), never directly on
    /// an async worker thread.
    pub fn new(client: GcpKmsClient) -> Self {
        Self {
            client,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub fn key_version(&self) -> &str {
        self.client.key_version()
    }
}

impl age::Identity for GcpKmsIdentity {
    fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
        if stanza.tag != GCP_KMS_STANZA_TAG || stanza.args != [self.client.key_version()] {
            return None;
        }
        let plaintext = self
            .runtime
            .block_on(self.client.asymmetric_decrypt(&stanza.body));
        let plaintext = match plaintext {
            Ok(plaintext) => plaintext,
            Err(err) => {
                return Some(Err(age::DecryptError::Io(io::Error::other(format!(
                    "GCP KMS asymmetric decrypt failed: {err:#}"
                )))));
            }
        };
        match <[u8; FILE_KEY_BYTES]>::try_from(plaintext.as_slice()) {
            Ok(bytes) => Some(Ok(FileKey::new(Box::new(bytes)))),
            Err(_) => Some(Err(age::DecryptError::KeyDecryptionFailed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePublicKey as _;

    const TEST_KEY_VERSION: &str =
        "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1";

    /// Test identity mirroring what GCP KMS `AsymmetricDecrypt` does server-side.
    struct LocalRsaOaepIdentity {
        private_key: RsaPrivateKey,
        key_version: String,
    }

    impl age::Identity for LocalRsaOaepIdentity {
        fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
            if stanza.tag != GCP_KMS_STANZA_TAG || stanza.args != [self.key_version.as_str()] {
                return None;
            }
            let plaintext = self
                .private_key
                .decrypt(Oaep::new::<Sha256>(), &stanza.body)
                .expect("RSA-OAEP unwrap failed");
            let bytes = <[u8; FILE_KEY_BYTES]>::try_from(plaintext.as_slice()).unwrap();
            Some(Ok(FileKey::new(Box::new(bytes))))
        }
    }

    #[test]
    fn kms_recipient_roundtrips_through_rsa_oaep_stanza() {
        let private_key = RsaPrivateKey::new(&mut rand_core06::OsRng, 2048).unwrap();
        let pem = private_key
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let recipient = GcpKmsRecipient::from_public_key_pem(
            &pem,
            "RSA_DECRYPT_OAEP_2048_SHA256",
            TEST_KEY_VERSION.to_owned(),
        )
        .unwrap();

        let encrypted = age::encrypt(&recipient, b"replay record").unwrap();

        let identity = LocalRsaOaepIdentity {
            private_key: private_key.clone(),
            key_version: TEST_KEY_VERSION.to_owned(),
        };
        let decrypted = age::decrypt(&identity, encrypted.as_slice()).unwrap();
        assert_eq!(decrypted, b"replay record");

        let other_version_identity = LocalRsaOaepIdentity {
            private_key,
            key_version: format!("{TEST_KEY_VERSION}0"),
        };
        let err = age::decrypt(&other_version_identity, encrypted.as_slice()).unwrap_err();
        assert!(matches!(err, age::DecryptError::NoMatchingKeys));
    }

    #[test]
    fn kms_recipient_rejects_non_oaep_sha256_algorithms() {
        let err = GcpKmsRecipient::from_public_key_pem(
            "irrelevant",
            "RSA_DECRYPT_OAEP_4096_SHA512",
            TEST_KEY_VERSION.to_owned(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported GCP KMS key algorithm")
        );
    }
}
