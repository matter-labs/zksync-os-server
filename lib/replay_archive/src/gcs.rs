use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveSession, ReplayArchiveStorage,
    ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use google_cloud_gax::error::rpc::Code;
use google_cloud_storage::client::{Storage, StorageControl};
use std::path::PathBuf;

/// Authentication mode for GCS replay archive access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GcsReplayArchiveAuthMode {
    /// Ambient authentication (works if the binary runs on Google Cloud, e.g. via workload
    /// identity). This is the primary mode this backend is built for.
    Authenticated,
    /// Authentication via a credentials file at the specified path.
    AuthenticatedWithCredentialFile(PathBuf),
    /// Anonymous access. This is only useful for read-only recovery from public buckets.
    Anonymous,
}

/// GCS replay archive configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcsReplayArchiveConfig {
    /// Name of the GCS bucket.
    pub bucket_base_url: String,
    pub auth_mode: GcsReplayArchiveAuthMode,
}

impl GcsReplayArchiveConfig {
    pub fn with_credential_file(
        bucket_base_url: impl Into<String>,
        gcs_credential_file_path: PathBuf,
    ) -> Self {
        Self {
            bucket_base_url: bucket_base_url.into(),
            auth_mode: GcsReplayArchiveAuthMode::AuthenticatedWithCredentialFile(
                gcs_credential_file_path,
            ),
        }
    }

    pub fn anonymous(bucket_base_url: impl Into<String>) -> Self {
        Self {
            bucket_base_url: bucket_base_url.into(),
            auth_mode: GcsReplayArchiveAuthMode::Anonymous,
        }
    }

    /// The bucket resource name in the `projects/_/buckets/{bucket}` format the client expects.
    fn bucket_resource(&self) -> String {
        format!("projects/_/buckets/{}", self.bucket_base_url)
    }
}

/// The two GCS clients used by this backend: `Storage` reads and writes object data over JSON;
/// `StorageControl` performs metadata operations (existence checks, listing) over gRPC. Both
/// retry transient errors internally, following the GCS retry guidance; writes are retried
/// because the `if_generation_match` precondition makes them idempotent.
#[derive(Clone, Debug)]
struct GcsClients {
    storage: Storage,
    control: StorageControl,
}

impl GcsClients {
    async fn new(auth_mode: &GcsReplayArchiveAuthMode) -> anyhow::Result<Self> {
        let credentials = match auth_mode {
            GcsReplayArchiveAuthMode::Authenticated => crate::gcp::ambient_credentials()?,
            GcsReplayArchiveAuthMode::AuthenticatedWithCredentialFile(path) => {
                crate::gcp::credentials_from_file(path)?
            }
            GcsReplayArchiveAuthMode::Anonymous => crate::gcp::anonymous_credentials(),
        };
        let storage = Storage::builder()
            .with_credentials(credentials.clone())
            .build()
            .await
            .context("failed to create replay archive GCS storage client")?;
        let control = StorageControl::builder()
            .with_credentials(credentials)
            .build()
            .await
            .context("failed to create replay archive GCS storage control client")?;
        Ok(Self { storage, control })
    }
}

fn is_not_found(err: &google_cloud_storage::Error) -> bool {
    err.http_status_code() == Some(404)
        || err
            .status()
            .is_some_and(|status| status.code == Code::NotFound)
}

/// GCS implementation of [`ReplayArchiveStorage`].
#[derive(Clone, Debug)]
pub struct GcsReplayArchiveStorage {
    config: GcsReplayArchiveConfig,
    session: ReplayArchiveSession,
    clients: GcsClients,
}

impl GcsReplayArchiveStorage {
    pub fn config(&self) -> &GcsReplayArchiveConfig {
        &self.config
    }

    pub fn session(&self) -> &ReplayArchiveSession {
        &self.session
    }

    fn object_key(&self, block_number: BlockNumber, block_hash: BlockHash) -> String {
        ReplayArchiveKey::new(self.session.clone(), block_number, block_hash).object_path()
    }

    async fn put_new_object(&self, key: &str, object: Vec<u8>) -> anyhow::Result<()> {
        self.clients
            .storage
            .write_object(
                self.config.bucket_resource(),
                key,
                bytes::Bytes::from(object),
            )
            // Succeeds only if no live version of this object exists yet — the GCS equivalent
            // of S3's `if_none_match("*")`, enforcing the append-only contract. This also makes
            // the upload safe to retry.
            .set_if_generation_match(0)
            .send_unbuffered()
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive GCS object gs://{}/{}",
                    self.config.bucket_base_url, key
                )
            })?;
        Ok(())
    }
}

#[async_trait]
impl ReplayArchiveStorage for GcsReplayArchiveStorage {
    type Config = GcsReplayArchiveConfig;

    async fn init(config: Self::Config, session: ReplayArchiveSession) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.bucket_base_url.is_empty(),
            "replay archive GCS bucket_base_url cannot be empty"
        );
        let clients = GcsClients::new(&config.auth_mode).await?;
        let storage = Self {
            config,
            session,
            clients,
        };
        storage
            .put_new_object(&crate::session_marker_key(&storage.session), Vec::new())
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive GCS session {}",
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
        let result = self
            .clients
            .control
            .get_object()
            .set_bucket(self.config.bucket_resource())
            .set_object(&key)
            .send()
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(err) if is_not_found(&err) => Ok(false),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to check replay archive GCS object gs://{}/{}",
                    self.config.bucket_base_url, key
                )
            }),
        }
    }
}

/// GCS implementation of [`ReplayArchiveStorageReader`].
#[derive(Clone, Debug)]
pub struct GcsReplayArchiveReader {
    config: GcsReplayArchiveConfig,
    clients: GcsClients,
}

impl GcsReplayArchiveReader {
    pub async fn new(config: GcsReplayArchiveConfig) -> anyhow::Result<Self> {
        let clients = GcsClients::new(&config.auth_mode).await?;
        Ok(Self { config, clients })
    }

    pub fn config(&self) -> &GcsReplayArchiveConfig {
        &self.config
    }
}

#[async_trait]
impl ReplayArchiveStorageReader for GcsReplayArchiveReader {
    async fn list_keys_page(
        &self,
        page_token: Option<String>,
    ) -> anyhow::Result<ReplayArchiveKeyPage> {
        let mut request = self
            .clients
            .control
            .list_objects()
            .set_parent(self.config.bucket_resource());
        if let Some(page_token) = page_token {
            request = request.set_page_token(page_token);
        }
        let response = request.send().await.with_context(|| {
            format!(
                "failed to list replay archive GCS objects in gs://{}",
                self.config.bucket_base_url
            )
        })?;

        let keys = response
            .objects
            .iter()
            .filter_map(|object| crate::parse_archive_object_key(&object.name))
            .collect();
        Ok(ReplayArchiveKeyPage {
            keys,
            next_page_token: Some(response.next_page_token).filter(|token| !token.is_empty()),
        })
    }

    async fn fetch_object(&self, key: &ReplayArchiveKey) -> anyhow::Result<Vec<u8>> {
        let object_key = key.object_path();
        let read = async {
            let mut reader = self
                .clients
                .storage
                .read_object(self.config.bucket_resource(), &object_key)
                .send()
                .await?;
            let mut object = Vec::new();
            while let Some(chunk) = reader.next().await.transpose()? {
                object.extend_from_slice(&chunk);
            }
            Ok::<_, google_cloud_storage::Error>(object)
        };
        read.await.with_context(|| {
            format!(
                "failed to read replay archive GCS object gs://{}/{}",
                self.config.bucket_base_url, object_key
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcs_config_builds_credential_file_auth_mode() {
        let config = GcsReplayArchiveConfig::with_credential_file(
            "bucket",
            "/path/to/credentials.json".into(),
        );

        assert_eq!(config.bucket_base_url, "bucket");
        assert_eq!(
            config.auth_mode,
            GcsReplayArchiveAuthMode::AuthenticatedWithCredentialFile(PathBuf::from(
                "/path/to/credentials.json"
            ))
        );
    }

    #[test]
    fn gcs_config_formats_bucket_resource() {
        let config = GcsReplayArchiveConfig::anonymous("bucket");

        assert_eq!(config.bucket_resource(), "projects/_/buckets/bucket");
    }
}
