use std::fmt;
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
use google_cloud_gax::{
    options::RequestOptionsBuilder as _,
    retry_policy::{Aip194Strict, RetryPolicy, RetryPolicyExt as _},
    retry_result::RetryResult,
    retry_state::RetryState,
};
use google_cloud_kms_v1::{
    client::KeyManagementService,
    model::{Digest, crypto_key_version::CryptoKeyVersionAlgorithm},
};
use k256::{
    ecdsa::{self, RecoveryId, VerifyingKey},
    pkcs8::DecodePublicKey as _,
};

/// Total number of attempts, including the initial request, for transient KMS failures.
const KMS_RETRY_ATTEMPTS: u32 = 4;

/// Per-attempt request timeout. The SDK sets no timeout by default, so without this a
/// stalled connection would hang a sign call (and the l1_sender pipeline) indefinitely.
const KMS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct KmsRetryPolicy;

impl RetryPolicy for KmsRetryPolicy {
    fn on_error(&self, state: &RetryState, error: google_cloud_gax::error::Error) -> RetryResult {
        use google_cloud_gax::error::rpc::Code;

        let retryable = error.is_timeout()
            || error
                .http_status_code()
                .is_some_and(|status| matches!(status, 408 | 429 | 500..=599))
            || error.status().is_some_and(|status| {
                matches!(
                    status.code,
                    Code::DeadlineExceeded | Code::ResourceExhausted | Code::Internal
                )
            });
        if state.idempotent && retryable {
            RetryResult::Continue(error)
        } else {
            Aip194Strict.on_error(state, error)
        }
    }
}

fn kms_retry_policy() -> impl RetryPolicy {
    KmsRetryPolicy.with_attempt_limit(KMS_RETRY_ATTEMPTS)
}

/// Google Cloud KMS-backed Ethereum signer.
#[derive(Clone)]
pub struct GcpKmsSigner {
    client: KeyManagementService,
    key_name: String,
    chain_id: Option<ChainId>,
    public_key: VerifyingKey,
    address: Address,
}

impl fmt::Debug for GcpKmsSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GcpKmsSigner")
            .field("key_name", &self.key_name)
            .field("chain_id", &self.chain_id)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl GcpKmsSigner {
    async fn new(
        client: KeyManagementService,
        key_name: String,
        chain_id: Option<ChainId>,
    ) -> anyhow::Result<Self> {
        let response = client
            .get_public_key()
            .set_name(&key_name)
            .with_idempotency(true)
            .send()
            .await
            .with_context(|| format!("failed to fetch GCP KMS public key for '{key_name}'"))?;

        anyhow::ensure!(
            response.name == key_name,
            "GCP KMS returned public key for '{}', expected '{key_name}'",
            response.name
        );
        anyhow::ensure!(
            response.algorithm == CryptoKeyVersionAlgorithm::EcSignSecp256K1Sha256,
            "GCP KMS key '{key_name}' uses {}, expected EC_SIGN_SECP256K1_SHA256",
            response.algorithm.name().unwrap_or("an unknown algorithm")
        );
        if let Some(expected_checksum) = response.pem_crc32c {
            anyhow::ensure!(
                i64::from(crc32c::crc32c(response.pem.as_bytes())) == expected_checksum,
                "GCP KMS public key checksum mismatch for '{key_name}'"
            );
        }

        let public_key = VerifyingKey::from_public_key_pem(&response.pem)
            .context("failed to parse GCP KMS secp256k1 public key PEM")?;
        let address = public_key_to_address(&public_key);

        Ok(Self {
            client,
            key_name,
            chain_id,
            public_key,
            address,
        })
    }

    async fn sign_digest(&self, digest: &B256) -> anyhow::Result<Signature> {
        let digest_checksum = i64::from(crc32c::crc32c(digest.as_slice()));
        let response = self
            .client
            .asymmetric_sign()
            .set_name(&self.key_name)
            .set_digest(Digest::new().set_sha256(digest.to_vec()))
            .set_digest_crc32c(digest_checksum)
            // Signing the same digest again is side-effect-free, so transient POST failures are
            // safe to retry through the SDK's bounded retry policy.
            .with_idempotency(true)
            .send()
            .await
            .with_context(|| format!("GCP KMS signing with '{}' failed", self.key_name))?;

        anyhow::ensure!(
            response.name == self.key_name,
            "GCP KMS signed with '{}', expected '{}'",
            response.name,
            self.key_name
        );
        anyhow::ensure!(
            response.verified_digest_crc32c,
            "GCP KMS did not verify the digest checksum for '{}'",
            self.key_name
        );
        let expected_checksum = response.signature_crc32c.with_context(|| {
            format!(
                "GCP KMS response omitted the signature checksum for '{}'",
                self.key_name
            )
        })?;
        anyhow::ensure!(
            i64::from(crc32c::crc32c(&response.signature)) == expected_checksum,
            "GCP KMS signature checksum mismatch for '{}'",
            self.key_name
        );

        let signature = ecdsa::Signature::from_der(&response.signature)
            .context("failed to decode GCP KMS DER signature")?;
        let signature = signature.normalize_s().unwrap_or(signature);
        let recovery_id = RecoveryId::trial_recovery_from_prehash(
            &self.public_key,
            digest.as_slice(),
            &signature,
        )
        .context("failed to recover parity from GCP KMS signature")?;
        Ok((signature, recovery_id).into())
    }
}

#[async_trait]
impl Signer for GcpKmsSigner {
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
impl TxSigner<Signature> for GcpKmsSigner {
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

/// Creates a signer using the official Google Cloud client and Application Default Credentials.
pub(crate) async fn create_gcp_signer(resource_name: &str) -> anyhow::Result<GcpKmsSigner> {
    let client = KeyManagementService::builder()
        .with_retry_policy(kms_retry_policy())
        .with_attempt_timeout(KMS_ATTEMPT_TIMEOUT)
        .build()
        .await
        .context("failed to create GCP KMS client using Application Default Credentials")?;

    GcpKmsSigner::new(client, resource_name.to_owned(), None)
        .await
        .with_context(|| format!("failed to initialize GCP KMS signer for '{resource_name}'"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use google_cloud_gax::{options::RequestOptions, response::Response};
    use google_cloud_kms_v1::model::{
        AsymmetricSignRequest, AsymmetricSignResponse, GetPublicKeyRequest, PublicKey,
    };
    use k256::{
        ecdsa::{SigningKey, signature::hazmat::PrehashSigner as _},
        pkcs8::{EncodePublicKey as _, LineEnding},
    };

    use super::*;

    const KEY_NAME: &str =
        "projects/project/locations/global/keyRings/ring/cryptoKeys/key/cryptoKeyVersions/1";
    const CLIENT_SECRET: &str = "credential-must-not-appear-in-debug";

    #[derive(Clone)]
    struct TestKmsStub {
        signing_key: SigningKey,
        algorithm: CryptoKeyVersionAlgorithm,
        corrupt_signature_checksum: bool,
        request_count: Arc<Mutex<(usize, usize)>>,
    }

    impl fmt::Debug for TestKmsStub {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(CLIENT_SECRET)
        }
    }

    impl TestKmsStub {
        fn new() -> Self {
            Self {
                signing_key: SigningKey::from_bytes((&[7_u8; 32]).into()).unwrap(),
                algorithm: CryptoKeyVersionAlgorithm::EcSignSecp256K1Sha256,
                corrupt_signature_checksum: false,
                request_count: Arc::new(Mutex::new((0, 0))),
            }
        }
    }

    impl google_cloud_kms_v1::stub::KeyManagementService for TestKmsStub {
        fn get_public_key(
            &self,
            request: GetPublicKeyRequest,
            options: RequestOptions,
        ) -> impl Future<Output = google_cloud_kms_v1::Result<Response<PublicKey>>> + Send {
            assert_eq!(request.name, KEY_NAME);
            assert_eq!(options.idempotent(), Some(true));
            self.request_count.lock().unwrap().0 += 1;

            let pem = self
                .signing_key
                .verifying_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap();
            let response = PublicKey::new()
                .set_name(KEY_NAME)
                .set_algorithm(self.algorithm.clone())
                .set_pem_crc32c(i64::from(crc32c::crc32c(pem.as_bytes())))
                .set_pem(pem);
            async move { Ok(Response::from(response)) }
        }

        fn asymmetric_sign(
            &self,
            request: AsymmetricSignRequest,
            options: RequestOptions,
        ) -> impl Future<Output = google_cloud_kms_v1::Result<Response<AsymmetricSignResponse>>> + Send
        {
            assert_eq!(request.name, KEY_NAME);
            assert_eq!(options.idempotent(), Some(true));
            let digest = request.digest.unwrap().sha256().unwrap().clone();
            assert_eq!(
                request.digest_crc32c,
                Some(i64::from(crc32c::crc32c(&digest)))
            );
            self.request_count.lock().unwrap().1 += 1;

            let signature: ecdsa::Signature = self.signing_key.sign_prehash(&digest).unwrap();
            let signature = signature.to_der().as_bytes().to_vec();
            let checksum =
                i64::from(crc32c::crc32c(&signature)) + i64::from(self.corrupt_signature_checksum);
            let response = AsymmetricSignResponse::new()
                .set_name(KEY_NAME)
                .set_verified_digest_crc32c(true)
                .set_signature_crc32c(checksum)
                .set_signature(signature);
            async move { Ok(Response::from(response)) }
        }
    }

    async fn signer(stub: TestKmsStub) -> anyhow::Result<GcpKmsSigner> {
        GcpKmsSigner::new(
            KeyManagementService::from_stub(stub),
            KEY_NAME.to_owned(),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn signs_hash_and_recovers_expected_address() {
        let stub = TestKmsStub::new();
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
    async fn rejects_unsupported_key_algorithm() {
        let mut stub = TestKmsStub::new();
        stub.algorithm = CryptoKeyVersionAlgorithm::EcSignP256Sha256;

        let error = signer(stub).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected EC_SIGN_SECP256K1_SHA256")
        );
    }

    #[tokio::test]
    async fn rejects_corrupt_signature_checksum() {
        let mut stub = TestKmsStub::new();
        stub.corrupt_signature_checksum = true;
        let signer = signer(stub).await.unwrap();

        let error = signer.sign_hash(&B256::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("signature checksum mismatch"));
    }

    #[tokio::test]
    async fn debug_does_not_include_client_details() {
        let signer = signer(TestKmsStub::new()).await.unwrap();

        let debug = format!("{signer:?}");
        assert!(debug.contains(KEY_NAME), "{debug}");
        assert!(!debug.contains(CLIENT_SECRET), "{debug}");
    }

    #[test]
    fn retry_policy_is_bounded_and_only_retries_transient_idempotent_errors() {
        use google_cloud_gax::error::{
            Error as GaxError,
            rpc::{Code, Status},
        };

        let rpc_error = |code| GaxError::service(Status::default().set_code(code));
        let http_error =
            |status| GaxError::service_with_http_metadata(Status::default(), Some(status), None);
        for error in [
            GaxError::timeout("test timeout"),
            rpc_error(Code::Unavailable),
            rpc_error(Code::DeadlineExceeded),
            rpc_error(Code::ResourceExhausted),
            rpc_error(Code::Internal),
            http_error(408),
            http_error(429),
            http_error(500),
            http_error(599),
        ] {
            assert!(
                kms_retry_policy()
                    .on_error(&RetryState::new(true), error)
                    .is_continue()
            );
        }
        assert!(
            kms_retry_policy()
                .on_error(
                    &RetryState::new(true).set_attempt_count(KMS_RETRY_ATTEMPTS - 1),
                    rpc_error(Code::Unavailable),
                )
                .is_continue()
        );
        assert!(
            kms_retry_policy()
                .on_error(
                    &RetryState::new(true).set_attempt_count(KMS_RETRY_ATTEMPTS),
                    rpc_error(Code::Unavailable),
                )
                .is_exhausted()
        );
        assert!(
            kms_retry_policy()
                .on_error(&RetryState::new(false), rpc_error(Code::ResourceExhausted),)
                .is_permanent()
        );
        assert!(
            kms_retry_policy()
                .on_error(&RetryState::new(true), rpc_error(Code::PermissionDenied),)
                .is_permanent()
        );
    }
}
