use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveStorage, ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use aws_config::{BehaviorVersion, ConfigLoader, Region, meta::region::RegionProviderChain};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::{Client, primitives::ByteStream};
use std::path::PathBuf;

/// Object metadata key recording which node archived the object; forensic only.
const ARCHIVED_BY_METADATA_KEY: &str = "archived-by";

/// Authentication mode for S3 replay archive access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum S3ReplayArchiveAuthMode {
    /// Authentication via a credentials file at the specified path.
    AuthenticatedWithCredentialFile(PathBuf),
    /// Anonymous access. This is only useful for read-only recovery from public buckets.
    Anonymous,
}

/// S3 replay archive configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ReplayArchiveConfig {
    /// Name or URL of the bucket.
    pub bucket_base_url: String,
    pub auth_mode: S3ReplayArchiveAuthMode,
    /// Allows overriding AWS S3 API endpoint, e.g. to use another S3-compatible store provider.
    pub endpoint: Option<String>,
    /// Allows specifying bucket region. If omitted, the SDK provider chain is used, falling back to `auto`.
    pub region: Option<String>,
}

impl S3ReplayArchiveConfig {
    pub fn with_credential_file(
        bucket_base_url: impl Into<String>,
        s3_credential_file_path: PathBuf,
    ) -> Self {
        Self {
            bucket_base_url: bucket_base_url.into(),
            auth_mode: S3ReplayArchiveAuthMode::AuthenticatedWithCredentialFile(
                s3_credential_file_path,
            ),
            endpoint: None,
            region: None,
        }
    }

    pub fn anonymous(bucket_base_url: impl Into<String>) -> Self {
        Self {
            bucket_base_url: bucket_base_url.into(),
            auth_mode: S3ReplayArchiveAuthMode::Anonymous,
            endpoint: None,
            region: None,
        }
    }
}

/// `If-None-Match: *` failures surface as HTTP 412 with code `PreconditionFailed`: a live
/// object already exists at the key.
fn is_precondition_failed<E, R>(err: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    matches!(
        err.as_service_error().and_then(|err| err.code()),
        Some("PreconditionFailed")
    )
}

/// S3 implementation of [`ReplayArchiveStorage`].
#[derive(Debug, Clone)]
pub struct S3ReplayArchiveStorage {
    config: S3ReplayArchiveConfig,
    writer_node_id: String,
    client: Client,
}

impl S3ReplayArchiveStorage {
    pub fn config(&self) -> &S3ReplayArchiveConfig {
        &self.config
    }

    fn object_key(block_number: BlockNumber, block_hash: BlockHash) -> String {
        ReplayArchiveKey::new(block_number, block_hash).object_path()
    }
}

#[async_trait]
impl ReplayArchiveStorage for S3ReplayArchiveStorage {
    type Config = S3ReplayArchiveConfig;

    async fn init(config: Self::Config, writer_node_id: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.bucket_base_url.is_empty(),
            "replay archive S3 bucket_base_url cannot be empty"
        );
        let client = create_s3_client(&config).await;
        Ok(Self {
            config,
            writer_node_id,
            client,
        })
    }

    async fn put_object_if_absent(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
    ) -> anyhow::Result<()> {
        let key = Self::object_key(block_number, block_hash);
        let result = self
            .client
            .put_object()
            .bucket(&self.config.bucket_base_url)
            .key(&key)
            .metadata(ARCHIVED_BY_METADATA_KEY, &self.writer_node_id)
            .if_none_match("*")
            .body(ByteStream::from(object))
            .send()
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) if is_precondition_failed(&err) => Ok(()),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to create replay archive S3 object s3://{}/{}",
                    self.config.bucket_base_url, key
                )
            }),
        }
    }

    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        let key = Self::object_key(block_number, block_hash);
        match self
            .client
            .head_object()
            .bucket(&self.config.bucket_base_url)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if matches!(err.as_service_error(), Some(err) if err.is_not_found()) => {
                Ok(false)
            }
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to check replay archive S3 object s3://{}/{}",
                    self.config.bucket_base_url, key
                )
            }),
        }
    }
}

/// S3 implementation of [`ReplayArchiveStorageReader`].
#[derive(Debug, Clone)]
pub struct S3ReplayArchiveReader {
    config: S3ReplayArchiveConfig,
    client: Client,
}

impl S3ReplayArchiveReader {
    pub async fn new(config: S3ReplayArchiveConfig) -> Self {
        let client = create_s3_client(&config).await;
        Self { config, client }
    }

    pub fn config(&self) -> &S3ReplayArchiveConfig {
        &self.config
    }
}

#[async_trait]
impl ReplayArchiveStorageReader for S3ReplayArchiveReader {
    async fn list_keys_page(
        &self,
        page_token: Option<String>,
    ) -> anyhow::Result<ReplayArchiveKeyPage> {
        let mut request = self
            .client
            .list_objects_v2()
            .bucket(&self.config.bucket_base_url);
        if let Some(token) = page_token {
            request = request.continuation_token(token);
        }
        let response = request.send().await.with_context(|| {
            format!(
                "failed to list replay archive S3 objects in s3://{}",
                self.config.bucket_base_url
            )
        })?;

        let mut keys = Vec::new();
        for object in response.contents() {
            let Some(object_key) = object.key() else {
                continue;
            };
            if let Some(key) = crate::parse_archive_object_key(object_key) {
                keys.push(key);
            }
        }
        Ok(ReplayArchiveKeyPage {
            keys,
            next_page_token: response.next_continuation_token().map(str::to_owned),
        })
    }

    async fn fetch_object(&self, key: &ReplayArchiveKey) -> anyhow::Result<Vec<u8>> {
        let object_key = key.object_path();
        let bytes = self
            .client
            .get_object()
            .bucket(&self.config.bucket_base_url)
            .key(&object_key)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to read replay archive S3 object s3://{}/{}",
                    self.config.bucket_base_url, object_key
                )
            })?
            .body
            .collect()
            .await
            .with_context(|| {
                format!(
                    "failed to collect replay archive S3 object s3://{}/{}",
                    self.config.bucket_base_url, object_key
                )
            })?
            .into_bytes()
            .to_vec();
        Ok(bytes)
    }
}

async fn create_s3_client(config: &S3ReplayArchiveConfig) -> Client {
    let region_provider = RegionProviderChain::first_try(config.region.clone().map(Region::new))
        .or_default_provider()
        .or_else(Region::new("auto"));
    let mut sdk_config = get_client_config(config.auth_mode.clone()).region(region_provider);
    if let Some(endpoint) = config.endpoint.clone() {
        tracing::info!(%endpoint, "using S3 endpoint defined in replay archive config");
        sdk_config = sdk_config.endpoint_url(endpoint);
    }
    let sdk_config = sdk_config.load().await;
    Client::new(&sdk_config)
}

fn get_client_config(auth_mode: S3ReplayArchiveAuthMode) -> ConfigLoader {
    match auth_mode {
        S3ReplayArchiveAuthMode::AuthenticatedWithCredentialFile(path) => {
            let profile_files = EnvConfigFiles::builder()
                .with_file(EnvConfigFileKind::Credentials, path)
                .build();
            aws_config::defaults(BehaviorVersion::latest()).profile_files(profile_files)
        }
        S3ReplayArchiveAuthMode::Anonymous => {
            aws_config::defaults(BehaviorVersion::latest()).no_credentials()
        }
    }
}
