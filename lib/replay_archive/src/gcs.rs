use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveSession, ReplayArchiveStorage,
    ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use google_cloud_auth::credentials::CredentialsFile;
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::list::ListObjectsRequest;
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use http::StatusCode;
use std::fmt;
use std::future::Future;
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
            // of S3's `if_none_match("*")`, enforcing the append-only contract. This also makes
            // the upload safe to retry.
            if_generation_match: Some(0),
            ..Default::default()
        };
        with_gcs_retries(
            "gcs_upload",
            &format!("creating replay archive object {key}"),
            || {
                self.client
                    .upload_object(&request, object.clone(), &upload_type)
            },
        )
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
        let result = with_gcs_retries(
            "gcs_check",
            &format!("checking replay archive object {key}"),
            || self.client.get_object(&request),
        )
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
        GcsReplayArchiveAuthMode::Authenticated => Ok(ClientConfig::default()
            .with_auth()
            .await
            .context(crate::GCP_CREDENTIALS_HINT)?),
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

/// Follows the GCS retry guidance: HTTP 408/429/5xx and transport-level failures are
/// transient; other client errors are permanent.
fn is_transient(err: &google_cloud_storage::http::Error) -> bool {
    match err {
        google_cloud_storage::http::Error::Response(response) => response.is_retriable(),
        google_cloud_storage::http::Error::HttpClient(err) => err
            .status()
            .is_none_or(|status| matches!(status.as_u16(), 408 | 429 | 500..=599)),
        _ => true,
    }
}

/// Retries a GCS call on transient failures with exponential backoff.
async fn with_gcs_retries<T, Fut>(
    operation_label: &'static str,
    description: &str,
    operation: impl Fn() -> Fut,
) -> Result<T, google_cloud_storage::http::Error>
where
    Fut: Future<Output = Result<T, google_cloud_storage::http::Error>>,
{
    crate::retry::with_transient_retries(operation_label, description, is_transient, operation)
        .await
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
    async fn list_keys_page(
        &self,
        page_token: Option<String>,
    ) -> anyhow::Result<ReplayArchiveKeyPage> {
        let request = ListObjectsRequest {
            bucket: self.config.bucket_base_url.clone(),
            page_token,
            ..Default::default()
        };
        let response = with_gcs_retries("gcs_list", "listing replay archive objects", || {
            self.client.list_objects(&request)
        })
        .await
        .with_context(|| {
            format!(
                "failed to list replay archive GCS objects in gs://{}",
                self.config.bucket_base_url
            )
        })?;

        let mut keys = Vec::new();
        for object in response.items.into_iter().flatten() {
            if let Some(key) = crate::parse_archive_object_key(&object.name) {
                keys.push(key);
            }
        }
        Ok(ReplayArchiveKeyPage {
            keys,
            next_page_token: response.next_page_token,
        })
    }

    async fn fetch_object(&self, key: &ReplayArchiveKey) -> anyhow::Result<Vec<u8>> {
        let object_key = key.object_path();
        let get_request = GetObjectRequest {
            bucket: self.config.bucket_base_url.clone(),
            object: object_key.clone(),
            ..Default::default()
        };
        let range = Range::default();
        with_gcs_retries(
            "gcs_download",
            &format!("downloading replay archive object {object_key}"),
            || self.client.download_object(&get_request, &range),
        )
        .await
        .with_context(|| {
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

    #[tokio::test]
    async fn gcs_writes_retry_transient_errors() {
        use httpmock::{Method, MockServer};

        let server = MockServer::start();
        let upload_mock = server.mock(|when, then| {
            when.method(Method::POST);
            then.status(503).body("service unavailable");
        });

        let mut client_config = ClientConfig::default().anonymous();
        client_config.storage_endpoint = server.base_url();
        let storage = GcsReplayArchiveStorage {
            config: GcsReplayArchiveConfig {
                bucket_base_url: "bucket".to_owned(),
                auth_mode: GcsReplayArchiveAuthMode::Anonymous,
            },
            session: ReplayArchiveSession::new(42, "node-a").unwrap(),
            client: Client::new(client_config),
        };

        let err = storage
            .append_object(7, BlockHash::ZERO, b"record".to_vec())
            .await
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("failed to create append-only"),
            "{err:#}"
        );
        assert_eq!(upload_mock.calls(), crate::retry::RETRY_ATTEMPTS as usize);
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
