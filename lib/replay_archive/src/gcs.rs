use crate::{
    ReplayArchiveKey, ReplayArchiveObject, ReplayArchiveObjectStream, ReplayArchiveSession,
    ReplayArchiveStorage, ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use google_cloud_auth::credentials::CredentialsFile;
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::list::ListObjectsRequest;
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use http::StatusCode;
use std::fmt;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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
}

/// GCS implementation of [`ReplayArchiveStorage`].
#[derive(Clone)]
pub struct GcsReplayArchiveStorage {
    config: GcsReplayArchiveConfig,
    session: ReplayArchiveSession,
    client: Client,
}

impl fmt::Debug for GcsReplayArchiveStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GcsReplayArchiveStorage")
            .field("config", &self.config)
            .field("session", &self.session)
            // Skip `client` as its representation may contain sensitive info.
            .finish_non_exhaustive()
    }
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
        let upload_type = UploadType::Simple(Media::new(key.to_owned()));
        let request = UploadObjectRequest {
            bucket: self.config.bucket_base_url.clone(),
            // Succeeds only if no live version of this object exists yet — the GCS equivalent
            // of S3's `if_none_match("*")`, enforcing the append-only contract.
            if_generation_match: Some(0),
            ..Default::default()
        };
        self.client
            .upload_object(&request, object, &upload_type)
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
        let client = create_gcs_client(&config).await?;
        let storage = Self {
            config,
            session,
            client,
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
        let request = GetObjectRequest {
            bucket: self.config.bucket_base_url.clone(),
            object: key.clone(),
            ..Default::default()
        };
        match self.client.get_object(&request).await {
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

/// Creates a GCS client for the given auth mode.
pub(crate) async fn create_gcs_client(config: &GcsReplayArchiveConfig) -> anyhow::Result<Client> {
    let client_config = get_client_config(config.auth_mode.clone())
        .await
        .context("failed to configure replay archive GCS client")?;
    Ok(Client::new(client_config))
}

async fn get_client_config(auth_mode: GcsReplayArchiveAuthMode) -> anyhow::Result<ClientConfig> {
    match auth_mode {
        GcsReplayArchiveAuthMode::AuthenticatedWithCredentialFile(path) => {
            // The `google_cloud_auth` API requests a string here (an owned one at that!), but
            // converts it to a `Path` internally.
            let path = path.into_os_string().into_string().map_err(|path| {
                anyhow::anyhow!("GCS credential file path is not valid UTF-8: {path:?}")
            })?;
            let cred_file = CredentialsFile::new_from_file(path).await?;
            Ok(ClientConfig::default().with_credentials(cred_file).await?)
        }
        GcsReplayArchiveAuthMode::Authenticated => Ok(ClientConfig::default().with_auth().await?),
        GcsReplayArchiveAuthMode::Anonymous => Ok(ClientConfig::default().anonymous()),
    }
}

fn is_not_found(err: &google_cloud_storage::http::Error) -> bool {
    match err {
        google_cloud_storage::http::Error::HttpClient(err) => err
            .status()
            .is_some_and(|status| status == StatusCode::NOT_FOUND),
        google_cloud_storage::http::Error::Response(response) => {
            response.code == StatusCode::NOT_FOUND.as_u16()
        }
        _ => false,
    }
}

/// GCS implementation of [`ReplayArchiveStorageReader`].
#[derive(Clone)]
pub struct GcsReplayArchiveReader {
    config: GcsReplayArchiveConfig,
    client: Client,
}

impl fmt::Debug for GcsReplayArchiveReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GcsReplayArchiveReader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GcsReplayArchiveReader {
    pub async fn new(config: GcsReplayArchiveConfig) -> anyhow::Result<Self> {
        let client = create_gcs_client(&config).await?;
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &GcsReplayArchiveConfig {
        &self.config
    }
}

#[async_trait]
impl ReplayArchiveStorageReader for GcsReplayArchiveReader {
    async fn list_objects(&self) -> ReplayArchiveObjectStream {
        let config = self.config.clone();
        let client = self.client.clone();
        let (sender, receiver) = mpsc::channel(crate::REPLAY_ARCHIVE_OBJECT_LIST_CHANNEL_SIZE);
        tokio::spawn(async move {
            if let Err(err) = list_objects(config, client, sender.clone()).await {
                let _ = sender.send(Err(err)).await;
            }
        });
        ReceiverStream::new(receiver).boxed()
    }
}

async fn list_objects(
    config: GcsReplayArchiveConfig,
    client: Client,
    sender: mpsc::Sender<anyhow::Result<ReplayArchiveObject>>,
) -> anyhow::Result<()> {
    let mut page_token = None;

    loop {
        let request = ListObjectsRequest {
            bucket: config.bucket_base_url.clone(),
            page_token: page_token.clone(),
            ..Default::default()
        };

        let response = client.list_objects(&request).await.with_context(|| {
            format!(
                "failed to list replay archive GCS objects in gs://{}",
                config.bucket_base_url
            )
        })?;

        for object in response.items.into_iter().flatten() {
            let object_key = object.name;
            let Some(key) = crate::parse_archive_object_key(&object_key)? else {
                continue;
            };

            let get_request = GetObjectRequest {
                bucket: config.bucket_base_url.clone(),
                object: object_key.clone(),
                ..Default::default()
            };
            let bytes = client
                .download_object(&get_request, &Range::default())
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay archive GCS object gs://{}/{}",
                        config.bucket_base_url, object_key
                    )
                })?;

            if sender
                .send(Ok(ReplayArchiveObject { key, bytes }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }

        let Some(next_token) = response.next_page_token else {
            break;
        };
        page_token = Some(next_token);
    }

    Ok(())
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

    #[cfg(unix)]
    #[tokio::test]
    async fn gcs_client_reports_non_utf8_credential_path() {
        use futures::FutureExt as _;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let config = GcsReplayArchiveConfig::with_credential_file(
            "bucket",
            PathBuf::from(OsString::from_vec(vec![0xff])),
        );

        let result = std::panic::AssertUnwindSafe(create_gcs_client(&config))
            .catch_unwind()
            .await;
        let err = match result
            .expect("non-UTF8 credential file path should return an error, not panic")
        {
            Ok(_) => panic!("non-UTF8 credential file path unexpectedly created a GCS client"),
            Err(err) => err,
        };

        assert!(format!("{err:#}").contains("GCS credential file path is not valid UTF-8"));
    }
}
