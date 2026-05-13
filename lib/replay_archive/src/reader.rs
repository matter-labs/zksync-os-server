use crate::ReplayArchiveKey;
use async_trait::async_trait;
use futures::stream::BoxStream;

pub type ReplayArchiveObjectStream = BoxStream<'static, anyhow::Result<ReplayArchiveObject>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArchiveObject {
    pub key: ReplayArchiveKey,
    pub bytes: Vec<u8>,
}

/// Read-side access to replay archive objects.
///
/// Implementations should hide backend-specific path parsing and return normalized archive objects.
#[async_trait]
pub trait ReplayArchiveStorageReader {
    /// Lists all stored replay archive objects.
    async fn list_objects(&self) -> ReplayArchiveObjectStream;
}
