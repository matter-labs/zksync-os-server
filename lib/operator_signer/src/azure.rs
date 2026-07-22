use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use alloy::{
    consensus::SignableTransaction,
    network::TxSigner,
    primitives::{Address, B256, ChainId, Signature},
    signers::utils::public_key_to_address,
};
use alloy_signer::{Error as SignerError, Signer, sign_transaction_with_chain_id};
use anyhow::Context as _;
use async_trait::async_trait;
use azure_core::credentials::{AccessToken, TokenCredential, TokenRequestOptions};
use azure_core::http::{ExponentialRetryOptions, RetryOptions};
use azure_identity::{
    DeveloperToolsCredential, ManagedIdentityCredential, WorkloadIdentityCredential,
};
use azure_security_keyvault_keys::{
    KeyClient, KeyClientOptions, ResourceId,
    models::{
        CurveName, KeyClientGetKeyOptions, KeyClientSignOptions, KeyType, SignParameters,
        SignatureAlgorithm,
    },
};
use k256::ecdsa::{self, RecoveryId, VerifyingKey};

/// Bounds each Key Vault operation, including the SDK's internal retries. The SDK sets no
/// transport read timeout, so without this a stalled connection would hang a sign call
/// (and the l1_sender pipeline) indefinitely.
const KEY_VAULT_TIMEOUT: Duration = Duration::from_secs(45);

/// Signing the same digest again is side-effect-free, so the SDK's bounded retries on
/// transient failures (408/429/5xx and transport errors) are safe. Kept under
/// `KEY_VAULT_TIMEOUT` so retries are not cut off mid-flight.
fn key_vault_retry_options() -> RetryOptions {
    RetryOptions::exponential(ExponentialRetryOptions {
        max_retries: 3,
        max_total_elapsed: azure_core::time::Duration::seconds(30),
        ..Default::default()
    })
}

/// Azure Key Vault (or Managed HSM)-backed Ethereum signer.
#[derive(Clone)]
pub struct AzureKmsSigner {
    // Arc because `KeyClient` is not `Clone`, but the signer is cloned for wallet registration.
    client: Arc<KeyClient>,
    key_name: String,
    key_version: String,
    /// Full key identifier URL, kept for logs and error messages.
    key_id: String,
    chain_id: Option<ChainId>,
    public_key: VerifyingKey,
    address: Address,
}

impl fmt::Debug for AzureKmsSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AzureKmsSigner")
            .field("key_id", &self.key_id)
            .field("chain_id", &self.chain_id)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl AzureKmsSigner {
    async fn new(
        client: Arc<KeyClient>,
        key_name: String,
        key_version: String,
        key_id: String,
        chain_id: Option<ChainId>,
    ) -> anyhow::Result<Self> {
        let key = with_timeout(
            &key_id,
            "get_key",
            client.get_key(
                &key_name,
                Some(KeyClientGetKeyOptions {
                    key_version: Some(key_version.clone()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .with_context(|| format!("failed to fetch Azure Key Vault public key for '{key_id}'"))?
        .into_model()
        .with_context(|| format!("failed to decode Azure Key Vault key '{key_id}'"))?;

        let jwk = key
            .key
            .with_context(|| format!("Azure Key Vault returned no key material for '{key_id}'"))?;
        verify_kid(jwk.kid.as_deref(), &key_name, &key_version, &key_id)?;
        anyhow::ensure!(
            matches!(jwk.kty, Some(KeyType::Ec | KeyType::EcHsm)),
            "Azure Key Vault key '{key_id}' is not an elliptic-curve key"
        );
        anyhow::ensure!(
            jwk.crv == Some(CurveName::P256K),
            "Azure Key Vault key '{key_id}' uses curve {:?}, expected P-256K (secp256k1)",
            jwk.crv
        );

        // SEC1 uncompressed point: 0x04 || x || y.
        let mut sec1 = [0_u8; 65];
        sec1[0] = 0x04;
        sec1[1..33].copy_from_slice(&coordinate(jwk.x, "x", &key_id)?);
        sec1[33..].copy_from_slice(&coordinate(jwk.y, "y", &key_id)?);
        let public_key = VerifyingKey::from_sec1_bytes(&sec1)
            .context("failed to parse Azure Key Vault secp256k1 public key")?;
        let address = public_key_to_address(&public_key);

        Ok(Self {
            client,
            key_name,
            key_version,
            key_id,
            chain_id,
            public_key,
            address,
        })
    }

    async fn sign_digest(&self, digest: &B256) -> anyhow::Result<Signature> {
        let parameters = SignParameters {
            algorithm: Some(SignatureAlgorithm::Es256K),
            value: Some(digest.to_vec()),
        };
        let response = with_timeout(
            &self.key_id,
            "sign",
            self.client.sign(
                &self.key_name,
                parameters.try_into()?,
                Some(KeyClientSignOptions {
                    key_version: Some(self.key_version.clone()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .with_context(|| format!("Azure Key Vault signing with '{}' failed", self.key_id))?
        .into_model()
        .with_context(|| {
            format!(
                "failed to decode Azure Key Vault sign response for '{}'",
                self.key_id
            )
        })?;

        verify_kid(
            response.kid.as_deref(),
            &self.key_name,
            &self.key_version,
            &self.key_id,
        )?;
        let raw = response.result.with_context(|| {
            format!(
                "Azure Key Vault sign response for '{}' omitted the signature",
                self.key_id
            )
        })?;

        // ES256K returns the raw 64-byte `r || s` encoding (IEEE P1363), not DER.
        let signature = ecdsa::Signature::from_slice(&raw)
            .context("failed to decode Azure Key Vault ES256K signature (expected 64-byte r||s)")?;
        let signature = signature.normalize_s().unwrap_or(signature);
        let recovery_id = RecoveryId::trial_recovery_from_prehash(
            &self.public_key,
            digest.as_slice(),
            &signature,
        )
        .context("failed to recover parity from Azure Key Vault signature")?;
        Ok((signature, recovery_id).into())
    }
}

#[async_trait]
impl Signer for AzureKmsSigner {
    async fn sign_hash(&self, hash: &B256) -> alloy::signers::Result<Signature> {
        self.sign_digest(hash).await.map_err(SignerError::other)
    }

    fn address(&self) -> Address {
        self.address
    }

    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id
    }

    fn set_chain_id(&mut self, chain_id: Option<ChainId>) {
        self.chain_id = chain_id;
    }
}

#[async_trait]
impl TxSigner<Signature> for AzureKmsSigner {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        transaction: &mut dyn SignableTransaction<Signature>,
    ) -> alloy::signers::Result<Signature> {
        sign_transaction_with_chain_id!(
            self,
            transaction,
            self.sign_hash(&transaction.signature_hash()).await
        )
    }
}

async fn with_timeout<T>(
    key_id: &str,
    operation: &str,
    future: impl Future<Output = azure_core::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(KEY_VAULT_TIMEOUT, future)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Azure Key Vault {operation} for '{key_id}' timed out after {KEY_VAULT_TIMEOUT:?}"
            )
        })?
        .map_err(Into::into)
}

/// Ensures a response refers to the exact key (vault, name, and version) this signer was
/// configured with, comparing parsed identifiers rather than raw URLs to stay robust to
/// case/formatting.
fn verify_kid(
    kid: Option<&str>,
    key_name: &str,
    key_version: &str,
    key_id: &str,
) -> anyhow::Result<()> {
    let kid = kid.with_context(|| {
        format!("Azure Key Vault response for '{key_id}' omitted the key identifier")
    })?;
    let resource: ResourceId = kid
        .parse()
        .with_context(|| format!("failed to parse Azure Key Vault key identifier '{kid}'"))?;
    let expected: ResourceId = key_id.parse().with_context(|| {
        format!("failed to parse configured Azure Key Vault key identifier '{key_id}'")
    })?;
    anyhow::ensure!(
        resource.vault_url.eq_ignore_ascii_case(&expected.vault_url)
            && resource.name == key_name
            && resource.version.as_deref() == Some(key_version),
        "Azure Key Vault responded for key '{kid}', expected '{key_id}'"
    );
    Ok(())
}

fn coordinate(value: Option<Vec<u8>>, name: &str, key_id: &str) -> anyhow::Result<[u8; 32]> {
    let value = value.with_context(|| {
        format!("Azure Key Vault key '{key_id}' is missing the '{name}' coordinate")
    })?;
    anyhow::ensure!(
        value.len() <= 32,
        "'{name}' coordinate of Azure Key Vault key '{key_id}' is {} bytes, expected at most 32",
        value.len()
    );
    // JWK integers may be shorter than the field size when leading bytes are zero.
    let mut out = [0_u8; 32];
    out[32 - value.len()..].copy_from_slice(&value);
    Ok(out)
}

/// Bounds each credential source's token attempt. On real Azure hosting, IMDS and token
/// exchanges answer in well under a second; outside Azure the IMDS address (169.254.169.254)
/// often black-holes instead of refusing, which would otherwise stall the whole chain.
const CREDENTIAL_SOURCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Tries each credential source in order and returns the first token successfully acquired.
/// The first source to succeed is locked in and used exclusively afterwards, so a hanging
/// earlier source (e.g. IMDS off-Azure) is only paid for once.
///
/// `azure_identity` ships no `DefaultAzureCredential`-style chain, so we compose the
/// production sources (AKS workload identity, VM/App Service managed identity) with the
/// developer-tools fallback (Azure CLI) ourselves. Sources whose environment prerequisites
/// are missing fail at construction and are skipped from the chain.
#[derive(Debug)]
struct ChainedTokenCredential {
    sources: Vec<Arc<dyn TokenCredential>>,
    selected: std::sync::OnceLock<usize>,
}

#[async_trait]
impl TokenCredential for ChainedTokenCredential {
    async fn get_token(
        &self,
        scopes: &[&str],
        options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        if let Some(&selected) = self.selected.get() {
            return self.sources[selected].get_token(scopes, options).await;
        }
        let mut last_error = None;
        for (index, source) in self.sources.iter().enumerate() {
            match tokio::time::timeout(
                CREDENTIAL_SOURCE_TIMEOUT,
                source.get_token(scopes, options.clone()),
            )
            .await
            {
                Ok(Ok(token)) => {
                    let _ = self.selected.set(index);
                    return Ok(token);
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "Azure credential source failed, trying next");
                    last_error = Some(error);
                }
                Err(_) => {
                    tracing::debug!(
                        "Azure credential source timed out after {CREDENTIAL_SOURCE_TIMEOUT:?}, trying next"
                    );
                    last_error = Some(azure_core::Error::with_message(
                        azure_core::error::ErrorKind::Credential,
                        format!("credential source timed out after {CREDENTIAL_SOURCE_TIMEOUT:?}"),
                    ));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            azure_core::Error::with_message(
                azure_core::error::ErrorKind::Credential,
                "no Azure credential sources available",
            )
        }))
    }
}

fn chained_credential() -> anyhow::Result<Arc<dyn TokenCredential>> {
    let sources: Vec<Arc<dyn TokenCredential>> = [
        WorkloadIdentityCredential::new(None).map(|c| c as _),
        ManagedIdentityCredential::new(None).map(|c| c as _),
        DeveloperToolsCredential::new(None).map(|c| c as _),
    ]
    .into_iter()
    .filter_map(Result::ok)
    .collect();
    anyhow::ensure!(
        !sources.is_empty(),
        "no Azure credential source available (workload identity, managed identity, or Azure CLI)"
    );
    Ok(Arc::new(ChainedTokenCredential {
        sources,
        selected: std::sync::OnceLock::new(),
    }))
}

/// Creates a signer from a full key identifier URL, e.g.
/// `https://{vault}.vault.azure.net/keys/{name}/{version}`.
pub(crate) async fn create_azure_signer(key_id: &str) -> anyhow::Result<AzureKmsSigner> {
    let resource: ResourceId = key_id
        .parse()
        .with_context(|| format!("invalid Azure Key Vault key identifier '{key_id}'"))?;
    // Without a pinned version, key rotation would silently change the operator address.
    let version = resource.version.clone().with_context(|| {
        format!("Azure Key Vault key identifier '{key_id}' must include a key version")
    })?;

    let mut options = KeyClientOptions::default();
    options.client_options.retry = key_vault_retry_options();
    let client = KeyClient::new(&resource.vault_url, chained_credential()?, Some(options))
        .context("failed to create Azure Key Vault client")?;

    AzureKmsSigner::new(
        Arc::new(client),
        resource.name,
        version,
        key_id.to_owned(),
        None,
    )
    .await
    .with_context(|| format!("failed to initialize Azure Key Vault signer for '{key_id}'"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use azure_core::http::{
        AsyncRawResponse, Body, Method, Request, StatusCode, Transport,
        headers::{AUTHORIZATION, Headers, WWW_AUTHENTICATE},
    };
    use azure_security_keyvault_keys::models::{JsonWebKey, Key};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use k256::ecdsa::{SigningKey, signature::hazmat::PrehashSigner as _};

    use super::*;

    const VAULT_URL: &str = "https://test-vault.vault.azure.net";
    const KEY_NAME: &str = "operator";
    const KEY_VERSION: &str = "0123456789abcdef";
    const CLIENT_SECRET: &str = "credential-must-not-appear-in-debug";

    fn key_id() -> String {
        format!("{VAULT_URL}/keys/{KEY_NAME}/{KEY_VERSION}")
    }

    struct StubCredential;

    impl fmt::Debug for StubCredential {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(CLIENT_SECRET)
        }
    }

    #[async_trait]
    impl TokenCredential for StubCredential {
        async fn get_token(
            &self,
            _scopes: &[&str],
            _options: Option<TokenRequestOptions<'_>>,
        ) -> azure_core::Result<AccessToken> {
            Ok(AccessToken::new(
                "test-token",
                azure_core::time::OffsetDateTime::now_utc() + azure_core::time::Duration::hours(1),
            ))
        }
    }

    #[derive(Debug)]
    struct TestKeyVault {
        signing_key: SigningKey,
        crv: CurveName,
        respond_kid: String,
        corrupt_signature: bool,
        request_count: Arc<Mutex<(usize, usize)>>,
    }

    impl TestKeyVault {
        fn new() -> Self {
            Self {
                signing_key: SigningKey::from_bytes((&[7_u8; 32]).into()).unwrap(),
                crv: CurveName::P256K,
                respond_kid: key_id(),
                corrupt_signature: false,
                request_count: Arc::new(Mutex::new((0, 0))),
            }
        }

        fn json_response(body: Vec<u8>) -> AsyncRawResponse {
            AsyncRawResponse::from_bytes(StatusCode::Ok, Headers::new(), body)
        }
    }

    #[async_trait]
    impl azure_core::http::HttpClient for TestKeyVault {
        async fn execute_request(&self, request: &Request) -> azure_core::Result<AsyncRawResponse> {
            // Key Vault uses challenge-based auth: the SDK probes with an unauthenticated,
            // body-less request first and expects a challenge naming the token scope.
            if request.headers().get_optional_str(&AUTHORIZATION).is_none() {
                let mut headers = Headers::new();
                headers.insert(
                    WWW_AUTHENTICATE,
                    r#"Bearer authorization="https://login.microsoftonline.com/test-tenant", resource="https://vault.azure.net""#,
                );
                return Ok(AsyncRawResponse::from_bytes(
                    StatusCode::Unauthorized,
                    headers,
                    Vec::new(),
                ));
            }
            assert!(
                request
                    .url()
                    .path()
                    .starts_with(&format!("/keys/{KEY_NAME}/{KEY_VERSION}"))
            );
            match request.method() {
                Method::Get => {
                    self.request_count.lock().unwrap().0 += 1;
                    let point = self.signing_key.verifying_key().to_encoded_point(false);
                    let jwk = JsonWebKey {
                        kid: Some(self.respond_kid.clone()),
                        kty: Some(KeyType::EcHsm),
                        crv: Some(self.crv.clone()),
                        x: Some(point.x().unwrap().to_vec()),
                        y: Some(point.y().unwrap().to_vec()),
                        ..Default::default()
                    };
                    let mut key = Key::default();
                    key.key = Some(jwk);
                    Ok(Self::json_response(serde_json::to_vec(&key).unwrap()))
                }
                Method::Post => {
                    assert!(request.url().path().ends_with("/sign"));
                    self.request_count.lock().unwrap().1 += 1;
                    let Body::Bytes(body) = request.body() else {
                        panic!("expected an in-memory request body");
                    };
                    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
                    assert_eq!(parsed["alg"], "ES256K");
                    let digest = URL_SAFE_NO_PAD
                        .decode(parsed["value"].as_str().unwrap())
                        .unwrap();

                    let signature: ecdsa::Signature =
                        self.signing_key.sign_prehash(&digest).unwrap();
                    let mut raw = signature.to_bytes().to_vec();
                    if self.corrupt_signature {
                        raw[10] ^= 0xff;
                    }
                    let response = serde_json::json!({
                        "kid": self.respond_kid,
                        "value": URL_SAFE_NO_PAD.encode(&raw),
                    });
                    Ok(Self::json_response(serde_json::to_vec(&response).unwrap()))
                }
                method => panic!("unexpected request method {method:?}"),
            }
        }
    }

    async fn signer(stub: TestKeyVault) -> anyhow::Result<AzureKmsSigner> {
        let mut options = KeyClientOptions::default();
        options.client_options.transport = Some(Transport::new(Arc::new(stub)));
        let client = KeyClient::new(VAULT_URL, Arc::new(StubCredential), Some(options)).unwrap();
        AzureKmsSigner::new(
            Arc::new(client),
            KEY_NAME.to_owned(),
            KEY_VERSION.to_owned(),
            key_id(),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn signs_hash_and_recovers_expected_address() {
        let stub = TestKeyVault::new();
        let request_count = stub.request_count.clone();
        let signer = signer(stub).await.unwrap();
        let digest = B256::repeat_byte(0x42);

        let signature = signer.sign_hash(&digest).await.unwrap();

        assert_eq!(
            signature.recover_address_from_prehash(&digest).unwrap(),
            Signer::address(&signer)
        );
        assert_eq!(*request_count.lock().unwrap(), (1, 1));
    }

    #[tokio::test]
    async fn rejects_unsupported_curve() {
        let mut stub = TestKeyVault::new();
        stub.crv = CurveName::P256;

        let error = signer(stub).await.unwrap_err();
        assert!(error.to_string().contains("expected P-256K"), "{error}");
    }

    #[tokio::test]
    async fn rejects_key_identifier_mismatch() {
        let mut stub = TestKeyVault::new();
        stub.respond_kid = format!("{VAULT_URL}/keys/other-key/{KEY_VERSION}");

        let error = signer(stub).await.unwrap_err();
        assert!(error.to_string().contains("responded for key"), "{error:#}");
    }

    #[tokio::test]
    async fn rejects_kid_from_different_vault() {
        let mut stub = TestKeyVault::new();
        stub.respond_kid =
            format!("https://other-vault.vault.azure.net/keys/{KEY_NAME}/{KEY_VERSION}");

        let error = signer(stub).await.unwrap_err();
        assert!(error.to_string().contains("responded for key"), "{error:#}");
    }

    #[tokio::test]
    async fn accepts_kid_with_different_url_casing() {
        let mut stub = TestKeyVault::new();
        stub.respond_kid =
            format!("HTTPS://TEST-VAULT.vault.azure.net/keys/{KEY_NAME}/{KEY_VERSION}");

        signer(stub).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_corrupt_signature() {
        let mut stub = TestKeyVault::new();
        stub.corrupt_signature = true;
        let signer = signer(stub).await.unwrap();

        let error = signer.sign_hash(&B256::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("recover parity"), "{error}");
    }

    #[tokio::test]
    async fn requires_pinned_key_version() {
        let error = create_azure_signer(&format!("{VAULT_URL}/keys/{KEY_NAME}"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("must include a key version"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn debug_does_not_include_client_details() {
        let signer = signer(TestKeyVault::new()).await.unwrap();

        let debug = format!("{signer:?}");
        assert!(debug.contains(&key_id()), "{debug}");
        assert!(!debug.contains(CLIENT_SECRET), "{debug}");
    }
}
