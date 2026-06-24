use crate::{
    ReplayArchiveKey, ReplayArchiveObject, ReplayArchiveObjectStream, ReplayArchiveSession,
    ReplayArchiveStorage, ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use aws_config::{BehaviorVersion, ConfigLoader, Region, meta::region::RegionProviderChain};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_s3::{Client, primitives::ByteStream};
use futures::StreamExt as _;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const LIST_OBJECTS_CHANNEL_SIZE: usize = 128;
const SESSION_MARKER_FILE_NAME: &str = ".session";

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

/// S3 implementation of [`ReplayArchiveStorage`].
#[derive(Debug, Clone)]
pub struct S3ReplayArchiveStorage {
    config: S3ReplayArchiveConfig,
    session: ReplayArchiveSession,
    client: Client,
}

impl S3ReplayArchiveStorage {
    pub fn config(&self) -> &S3ReplayArchiveConfig {
        &self.config
    }

    pub fn session(&self) -> &ReplayArchiveSession {
        &self.session
    }

    fn object_key(&self, block_number: BlockNumber, block_hash: BlockHash) -> String {
        ReplayArchiveKey::new(self.session.clone(), block_number, block_hash).object_path()
    }

    fn session_marker_key(&self) -> String {
        format!("{}/{}", self.session, SESSION_MARKER_FILE_NAME)
    }

    async fn put_new_object(&self, key: &str, object: Vec<u8>) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.config.bucket_base_url)
            .key(key)
            .if_none_match("*")
            .body(ByteStream::from(object))
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive S3 object s3://{}/{}",
                    self.config.bucket_base_url, key
                )
            })?;
        Ok(())
    }
}

#[async_trait]
impl ReplayArchiveStorage for S3ReplayArchiveStorage {
    type Config = S3ReplayArchiveConfig;

    async fn init(config: Self::Config, session: ReplayArchiveSession) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.bucket_base_url.is_empty(),
            "replay archive S3 bucket_base_url cannot be empty"
        );
        let client = create_s3_client(&config).await;
        let storage = Self {
            config,
            session,
            client,
        };
        storage
            .put_new_object(&storage.session_marker_key(), Vec::new())
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive S3 session {}",
                    storage.session
                )
            })?;
        Ok(storage)
    }

    async fn append_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.put_new_object(&self.object_key(block_number, block_hash), object)
            .await
    }

    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        let key = self.object_key(block_number, block_hash);
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
    async fn list_objects(&self) -> ReplayArchiveObjectStream {
        let config = self.config.clone();
        let client = self.client.clone();
        let (sender, receiver) = mpsc::channel(LIST_OBJECTS_CHANNEL_SIZE);
        tokio::spawn(async move {
            if let Err(err) = list_objects(config, client, sender.clone()).await {
                let _ = sender.send(Err(err)).await;
            }
        });
        ReceiverStream::new(receiver).boxed()
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

async fn list_objects(
    config: S3ReplayArchiveConfig,
    client: Client,
    sender: mpsc::Sender<anyhow::Result<ReplayArchiveObject>>,
) -> anyhow::Result<()> {
    let mut continuation_token = None;

    loop {
        let mut request = client.list_objects_v2().bucket(&config.bucket_base_url);
        if let Some(token) = &continuation_token {
            request = request.continuation_token(token);
        }

        let response = request.send().await.with_context(|| {
            format!(
                "failed to list replay archive S3 objects in s3://{}",
                config.bucket_base_url
            )
        })?;

        for object in response.contents() {
            let Some(object_key) = object.key() else {
                continue;
            };
            let Some(key) = parse_s3_archive_key(object_key)? else {
                continue;
            };
            let bytes = client
                .get_object()
                .bucket(&config.bucket_base_url)
                .key(object_key)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay archive S3 object s3://{}/{}",
                        config.bucket_base_url, object_key
                    )
                })?
                .body
                .collect()
                .await
                .with_context(|| {
                    format!(
                        "failed to collect replay archive S3 object s3://{}/{}",
                        config.bucket_base_url, object_key
                    )
                })?
                .into_bytes()
                .to_vec();

            if sender
                .send(Ok(ReplayArchiveObject { key, bytes }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }

        let Some(next_token) = response.next_continuation_token() else {
            break;
        };
        continuation_token = Some(next_token.to_owned());
    }

    Ok(())
}

fn parse_s3_archive_key(object_key: &str) -> anyhow::Result<Option<ReplayArchiveKey>> {
    let parts = object_key.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts[2] == SESSION_MARKER_FILE_NAME {
        return Ok(None);
    }

    let session = parts[0]
        .parse::<ReplayArchiveSession>()
        .with_context(|| format!("failed to parse replay archive S3 session in {object_key}"))?;
    let block_number = parts[1].parse::<BlockNumber>().with_context(|| {
        format!("failed to parse replay archive S3 block number in {object_key}")
    })?;
    let block_hash = parts[2]
        .parse::<BlockHash>()
        .with_context(|| format!("failed to parse replay archive S3 block hash in {object_key}"))?;

    Ok(Some(ReplayArchiveKey::new(
        session,
        block_number,
        block_hash,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_block_hash;
    use alloy::primitives::B256;

    #[test]
    fn s3_object_key_uses_archive_layout() {
        let session = ReplayArchiveSession::new(42, "node-a").unwrap();
        let key = ReplayArchiveKey::new(session, 7, B256::ZERO);

        assert_eq!(
            key.object_path(),
            "42-node-a/7/0x0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn s3_parser_skips_session_marker_and_non_archive_keys() {
        assert!(parse_s3_archive_key("other/key").unwrap().is_none());
        assert!(
            parse_s3_archive_key("42-node-a/.session")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn s3_parser_accepts_archive_key() {
        let block_hash = B256::with_last_byte(1);
        let object_key = format!("42-node-a/7/{}", format_block_hash(block_hash));

        let parsed = parse_s3_archive_key(&object_key).unwrap().unwrap();

        assert_eq!(
            parsed,
            ReplayArchiveKey::new(
                ReplayArchiveSession::new(42, "node-a").unwrap(),
                7,
                block_hash
            )
        );
    }

    #[test]
    fn s3_config_builds_credential_file_auth_mode() {
        let config =
            S3ReplayArchiveConfig::with_credential_file("bucket", "/path/to/credentials".into());

        assert_eq!(config.bucket_base_url, "bucket");
        assert_eq!(
            config.auth_mode,
            S3ReplayArchiveAuthMode::AuthenticatedWithCredentialFile(PathBuf::from(
                "/path/to/credentials"
            ))
        );
    }
}
