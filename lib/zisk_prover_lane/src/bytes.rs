//! Typed wrapper for the batch-level second proof-system (ZiSK) payload.

/// Serialized batch-level ZiSK `BatchInput` (bincode).
///
/// The batch assembly produces one value per batch. `seal_batch` returns it and
/// the batcher hands it to [`crate::ZiskJobManager::add_job`], which drives the
/// ZiSK proving lane. The node integration re-exports this type from its own
/// `zisk_bytes` module, so both sides name the one payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZiskBatchBytes(pub Vec<u8>);

impl ZiskBatchBytes {
    /// Return the wrapped bytes and consume the wrapper.
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    /// Return the wrapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Return the byte length.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Report whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for ZiskBatchBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}
