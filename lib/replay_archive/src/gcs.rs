use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveStorage, ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use google_cloud_auth::credentials::anonymous;
use google_cloud_gax::error::rpc::Code;
use google_cloud_gax::options::RequestOptionsBuilder as _;
use google_cloud_storage::client::{Storage, StorageControl};
use std::fmt;

/// Object metadata key recording which node archived the object; forensic only.
const ARCHIVED_BY_METADATA_KEY: &str = "archived-by";

/// Authentication mode for GCS replay archive access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GcsReplayArchiveAuthMode {
    /// Application Default Credentials. This supports GKE Workload Identity, external workload
    /// identity federation configured via `GOOGLE_APPLICATION_CREDENTIALS`, and local ADC.
    Authenticated,
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

/// The two GCS clients this backend uses: `Storage` reads/writes object data over JSON,
/// `StorageControl` does metadata ops (existence checks, listing) over gRPC. Both retry transient
/// errors internally; writes stay idempotent via the `if_generation_match` precondition.
#[derive(Clone)]
struct GcsClients {
    storage: Storage,
    control: StorageControl,
}

impl GcsClients {
    async fn new(auth_mode: &GcsReplayArchiveAuthMode) -> anyhow::Result<Self> {
        let (storage_builder, control_builder) = match auth_mode {
            GcsReplayArchiveAuthMode::Authenticated => {
                (Storage::builder(), StorageControl::builder())
            }
            GcsReplayArchiveAuthMode::Anonymous => {
                let credentials = anonymous::Builder::new().build();
                (
                    Storage::builder().with_credentials(credentials.clone()),
                    StorageControl::builder().with_credentials(credentials),
                )
            }
        };
        let storage = storage_builder
            .build()
            .await
            .context("failed to create replay archive GCS storage client")?;
        let control = control_builder
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

/// `if_generation_match(0)` failures surface as HTTP 412 over JSON and `FAILED_PRECONDITION`
/// over gRPC; both mean a live object already exists at the key.
fn is_precondition_failed(err: &google_cloud_storage::Error) -> bool {
    err.http_status_code() == Some(412)
        || err
            .status()
            .is_some_and(|status| status.code == Code::FailedPrecondition)
}

/// GCS implementation of [`ReplayArchiveStorage`].
#[derive(Clone)]
pub struct GcsReplayArchiveStorage {
    config: GcsReplayArchiveConfig,
    writer_node_id: String,
    clients: GcsClients,
}

impl fmt::Debug for GcsReplayArchiveStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GcsReplayArchiveStorage")
            .field("config", &self.config)
            .field("writer_node_id", &self.writer_node_id)
            .finish_non_exhaustive()
    }
}

impl GcsReplayArchiveStorage {
    pub fn config(&self) -> &GcsReplayArchiveConfig {
        &self.config
    }

    fn object_key(block_number: BlockNumber, block_hash: BlockHash) -> String {
        ReplayArchiveKey::new(block_number, block_hash).object_path()
    }
}

#[async_trait]
impl ReplayArchiveStorage for GcsReplayArchiveStorage {
    type Config = GcsReplayArchiveConfig;

    async fn init(config: Self::Config, writer_node_id: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.bucket_base_url.is_empty(),
            "replay archive GCS bucket_base_url cannot be empty"
        );
        let clients = GcsClients::new(&config.auth_mode).await?;
        Ok(Self {
            config,
            writer_node_id,
            clients,
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
            .clients
            .storage
            .write_object(
                self.config.bucket_resource(),
                &key,
                bytes::Bytes::from(object),
            )
            .set_metadata([(ARCHIVED_BY_METADATA_KEY, &self.writer_node_id)])
            // Succeeds only if no live version of this object exists yet — the GCS equivalent
            // of S3's `if_none_match("*")`. Exactly one concurrent writer wins; the rest see a
            // precondition failure. This also makes the upload safe to retry.
            .set_if_generation_match(0)
            .send_unbuffered()
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) if is_precondition_failed(&err) => Ok(()),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to create replay archive GCS object gs://{}/{}",
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
        let result = self
            .clients
            .control
            .get_object()
            .set_bucket(self.config.bucket_resource())
            .set_object(&key)
            .with_idempotency(true)
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
#[derive(Clone)]
pub struct GcsReplayArchiveReader {
    config: GcsReplayArchiveConfig,
    clients: GcsClients,
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
        let response = request
            .with_idempotency(true)
            .send()
            .await
            .with_context(|| {
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
    use google_cloud_gax::options::RequestOptions;
    use google_cloud_gax::response::Response;
    use google_cloud_storage::model::{
        GetObjectRequest, ListObjectsRequest, ListObjectsResponse, Object,
    };
    use std::fmt;

    struct IdempotencyCheckingControl;

    const TEST_CLIENT_SECRET: &str = "credential-must-not-appear-in-debug";
    impl fmt::Debug for IdempotencyCheckingControl {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(TEST_CLIENT_SECRET)
        }
    }

    impl google_cloud_storage::stub::StorageControl for IdempotencyCheckingControl {
        async fn get_object(
            &self,
            _request: GetObjectRequest,
            options: RequestOptions,
        ) -> google_cloud_gax::Result<Response<Object>> {
            assert_eq!(options.idempotent(), Some(true));
            Ok(Response::from(Object::new()))
        }

        async fn list_objects(
            &self,
            _request: ListObjectsRequest,
            options: RequestOptions,
        ) -> google_cloud_gax::Result<Response<ListObjectsResponse>> {
            assert_eq!(options.idempotent(), Some(true));
            Ok(Response::from(ListObjectsResponse::new()))
        }
    }

    async fn idempotency_checking_clients() -> GcsClients {
        let storage = Storage::builder()
            .with_credentials(anonymous::Builder::new().build())
            .build()
            .await
            .unwrap();
        let control = StorageControl::from_stub(IdempotencyCheckingControl);
        GcsClients { storage, control }
    }

    #[test]
    fn gcs_config_formats_bucket_resource() {
        let config = GcsReplayArchiveConfig::anonymous("bucket");

        assert_eq!(config.bucket_resource(), "projects/_/buckets/bucket");
    }

    #[tokio::test]
    async fn contains_object_marks_request_idempotent() {
        let storage = GcsReplayArchiveStorage {
            config: GcsReplayArchiveConfig::anonymous("bucket"),
            writer_node_id: "node-a".to_owned(),
            clients: idempotency_checking_clients().await,
        };

        assert!(storage.contains_object(7, BlockHash::ZERO).await.unwrap());
    }

    #[tokio::test]
    async fn list_keys_page_marks_request_idempotent() {
        let reader = GcsReplayArchiveReader {
            config: GcsReplayArchiveConfig::anonymous("bucket"),
            clients: idempotency_checking_clients().await,
        };

        assert_eq!(
            reader.list_keys_page(None).await.unwrap(),
            ReplayArchiveKeyPage {
                keys: Vec::new(),
                next_page_token: None,
            }
        );
    }

    #[tokio::test]
    async fn gcs_debug_does_not_include_client_details() {
        let clients = idempotency_checking_clients().await;
        let storage = GcsReplayArchiveStorage {
            config: GcsReplayArchiveConfig::anonymous("bucket"),
            writer_node_id: "node-a".to_owned(),
            clients: clients.clone(),
        };
        let reader = GcsReplayArchiveReader {
            config: GcsReplayArchiveConfig::anonymous("bucket"),
            clients,
        };

        let storage_debug = format!("{storage:?}");
        assert!(storage_debug.contains("bucket"), "{storage_debug}");
        assert!(
            !storage_debug.contains(TEST_CLIENT_SECRET),
            "{storage_debug}"
        );

        let reader_debug = format!("{reader:?}");
        assert!(reader_debug.contains("bucket"), "{reader_debug}");
        assert!(!reader_debug.contains(TEST_CLIENT_SECRET), "{reader_debug}");
    }
}
