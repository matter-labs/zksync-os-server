//! age recipient/identity backed by a GCP KMS asymmetric key.
//!
//! Encryption wraps the per-file age file key with the RSA-OAEP-SHA256 public key of a KMS
//! `ASYMMETRIC_DECRYPT` key version and is fully local: the node only needs the public key,
//! fetched once at startup. Decryption unwraps the file key with one KMS `AsymmetricDecrypt`
//! call per object; the private key never leaves KMS.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use age_core::format::{FILE_KEY_BYTES, FileKey, Stanza};
use age_core::secrecy::ExposeSecret as _;
use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use google_cloud_auth::credentials::CredentialsFile;
use google_cloud_auth::project::Config as AuthConfig;
use google_cloud_auth::token::DefaultTokenSourceProvider;
use google_cloud_token::{TokenSource, TokenSourceProvider as _};
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Oaep, RsaPublicKey};
use serde::Deserialize;
use sha2::Sha256;

/// `cloud-platform` must come along with the KMS scope: under workload identity federation
/// the federated STS token itself calls `iamcredentials.generateAccessToken`, which rejects
/// tokens that only carry service-specific scopes.
const KMS_SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/cloudkms",
];
const KMS_ENDPOINT: &str = "https://cloudkms.googleapis.com/v1";
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

/// Minimal GCP KMS REST client covering the two methods the replay archive needs.
#[derive(Clone)]
pub struct GcpKmsClient {
    http: reqwest::Client,
    token_source: Arc<dyn TokenSource>,
    key_version: String,
    endpoint: String,
}

impl fmt::Debug for GcpKmsClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GcpKmsClient")
            .field("key_version", &self.key_version)
            // Skip `http` / `token_source` as their representations may contain sensitive info.
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicKeyResponse {
    pem: String,
    algorithm: String,
}

#[derive(Debug, Deserialize)]
struct AsymmetricDecryptResponse {
    plaintext: String,
}

impl GcpKmsClient {
    pub async fn new(config: &GcpKmsConfig) -> anyhow::Result<Self> {
        let auth_config = AuthConfig {
            audience: None,
            scopes: Some(&KMS_SCOPES),
            sub: None,
        };
        let provider = match &config.auth_mode {
            GcpKmsAuthMode::AuthenticatedWithCredentialFile(path) => {
                let path = path
                    .to_str()
                    .with_context(|| {
                        format!("credentials file path {} is not UTF-8", path.display())
                    })?
                    .to_owned();
                let credentials = CredentialsFile::new_from_file(path)
                    .await
                    .context("failed to read GCP KMS credentials file")?;
                DefaultTokenSourceProvider::new_with_credentials(auth_config, Box::new(credentials))
                    .await
                    .context("failed to initialize GCP KMS token source from credentials file")?
            }
            GcpKmsAuthMode::Authenticated => DefaultTokenSourceProvider::new(auth_config)
                .await
                .context(crate::GCP_CREDENTIALS_HINT)
                .context("failed to initialize GCP KMS token source")?,
        };
        Ok(Self {
            http: reqwest::Client::new(),
            token_source: provider.token_source(),
            key_version: config.key_version.clone(),
            endpoint: KMS_ENDPOINT.to_owned(),
        })
    }

    pub fn key_version(&self) -> &str {
        &self.key_version
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<String>,
    ) -> anyhow::Result<String> {
        let result = crate::retry::with_transient_retries(
            "kms",
            &format!("requesting {url} from GCP KMS"),
            KmsRequestError::is_transient,
            || async {
                let token = self
                    .token_source
                    .token()
                    .await
                    .map_err(|err| KmsRequestError::Token(err.to_string()))?;
                let mut request = self
                    .http
                    .request(method.clone(), &url)
                    .header("authorization", token);
                if let Some(body) = &body {
                    request = request
                        .header("content-type", "application/json")
                        .body(body.clone());
                }
                let response = request.send().await.map_err(KmsRequestError::Send)?;
                let status = response.status();
                let body = response.text().await.map_err(KmsRequestError::Read)?;
                if status.is_success() {
                    Ok(body)
                } else {
                    Err(KmsRequestError::Status { status, body })
                }
            },
        )
        .await;
        match result {
            Ok(body) => Ok(body),
            Err(KmsRequestError::Token(message)) => Err(anyhow::anyhow!(message))
                .context(crate::GCP_CREDENTIALS_HINT)
                .context("failed to obtain GCP access token"),
            Err(err @ KmsRequestError::Send(_)) => {
                Err(err).with_context(|| format!("GCP KMS request to {url} failed"))
            }
            Err(err @ KmsRequestError::Read(_)) => {
                Err(err).with_context(|| format!("failed to read GCP KMS response from {url}"))
            }
            Err(KmsRequestError::Status { status, body }) => {
                anyhow::bail!("GCP KMS request to {url} returned {status}: {body}")
            }
        }
    }

    /// Fetches the public key of the key version, returning `(pem, algorithm)`.
    pub async fn get_public_key(&self) -> anyhow::Result<(String, String)> {
        let url = format!("{}/{}/publicKey", self.endpoint, self.key_version);
        let body = self.request(reqwest::Method::GET, url, None).await?;
        let response: PublicKeyResponse =
            serde_json::from_str(&body).context("failed to decode GCP KMS public key response")?;
        Ok((response.pem, response.algorithm))
    }

    /// Decrypts data encrypted with the public key of the key version.
    pub async fn asymmetric_decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/{}:asymmetricDecrypt", self.endpoint, self.key_version);
        let request = serde_json::json!({ "ciphertext": BASE64.encode(ciphertext) }).to_string();
        let body = self
            .request(reqwest::Method::POST, url, Some(request))
            .await?;
        let response: AsymmetricDecryptResponse = serde_json::from_str(&body)
            .context("failed to decode GCP KMS asymmetric decrypt response")?;
        BASE64
            .decode(response.plaintext)
            .context("failed to decode GCP KMS asymmetric decrypt plaintext")
    }
}

/// Classified outcome of a single KMS HTTP attempt, so that only transient failures are
/// retried.
#[derive(Debug, thiserror::Error)]
enum KmsRequestError {
    #[error("failed to obtain GCP access token: {0}")]
    Token(String),
    #[error(transparent)]
    Send(reqwest::Error),
    #[error(transparent)]
    Read(reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl KmsRequestError {
    /// Token errors are usually configuration problems and are not retried; transport
    /// failures and HTTP 408/429/5xx are transient.
    fn is_transient(&self) -> bool {
        match self {
            Self::Token(_) => false,
            Self::Send(_) | Self::Read(_) => true,
            Self::Status { status, .. } => matches!(status.as_u16(), 408 | 429 | 500..=599),
        }
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

    #[derive(Debug)]
    struct FakeTokenSource;

    #[async_trait::async_trait]
    impl TokenSource for FakeTokenSource {
        async fn token(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            Ok("Bearer test-token".to_owned())
        }
    }

    fn test_client(endpoint: String) -> GcpKmsClient {
        GcpKmsClient {
            http: reqwest::Client::new(),
            token_source: Arc::new(FakeTokenSource),
            key_version: TEST_KEY_VERSION.to_owned(),
            endpoint,
        }
    }

    #[tokio::test]
    async fn kms_client_retries_transient_errors() {
        use httpmock::{Method, MockServer};

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(Method::POST);
            then.status(503).body("service unavailable");
        });

        let err = test_client(server.base_url())
            .asymmetric_decrypt(b"ciphertext")
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("503"), "{err:#}");
        assert_eq!(mock.calls(), crate::retry::RETRY_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn kms_client_does_not_retry_permanent_errors() {
        use httpmock::{Method, MockServer};

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(Method::POST);
            then.status(400).body("bad request");
        });

        let err = test_client(server.base_url())
            .asymmetric_decrypt(b"ciphertext")
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("400"), "{err:#}");
        assert_eq!(mock.calls(), 1);
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
