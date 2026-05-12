use crate::ReplayArchiveKey;
use async_trait::async_trait;
use futures::stream::BoxStream;

pub type ReplayArchiveObjectStream = BoxStream<'static, anyhow::Result<ReplayArchiveKey>>;

/// Read-side access to replay archive objects.
///
/// Implementations should hide backend-specific path parsing and return normalized archive keys.
#[async_trait]
pub trait ReplayArchiveStorageReader {
    /// Lists all stored replay archive objects.
    async fn list_objects(&self) -> anyhow::Result<ReplayArchiveObjectStream>;

    /// Reads an object by its normalized archive key.
    async fn read_object(&self, key: &ReplayArchiveKey) -> anyhow::Result<Vec<u8>>;
}
