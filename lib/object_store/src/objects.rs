//! Stored objects.

use crate::raw::{BoxedError, Bucket, ObjectStore, ObjectStoreError};

/// Object that can be stored in an [`ObjectStore`].
pub trait StoredObject: Sized {
    /// Bucket in which values are stored.
    const BUCKET: Bucket;
    /// Logical unique key for the object. The lifetime param allows defining keys
    /// that borrow data; see [`CircuitKey`] for an example.
    type Key<'a>: Copy;

    /// Fallback key for the object. If the object is not found, the fallback key is used.
    fn fallback_key(_key: Self::Key<'_>) -> Option<String> {
        None
    }

    /// Encodes the object key to a string.
    fn encode_key(key: Self::Key<'_>) -> String;

    /// Serializes a value to a blob.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    fn serialize(&self) -> Result<Vec<u8>, BoxedError>;

    /// Deserializes a value from the blob.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    fn deserialize(bytes: Vec<u8>) -> Result<Self, BoxedError>;
}

/// Derives [`StoredObject::serialize()`] and [`StoredObject::deserialize()`] using
/// the `bincode` (de)serializer. Should be used in `impl StoredObject` blocks.
#[macro_export]
macro_rules! serialize_using_bincode {
    () => {
        fn serialize(
            &self,
        ) -> std::result::Result<std::vec::Vec<u8>, $crate::_reexports::BoxedError> {
            $crate::bincode::serialize(self).map_err(std::convert::From::from)
        }

        fn deserialize(
            bytes: std::vec::Vec<u8>,
        ) -> std::result::Result<Self, $crate::_reexports::BoxedError> {
            $crate::bincode::deserialize(&bytes).map_err(std::convert::From::from)
        }
    };
}

impl dyn ObjectStore + '_ {
    /// Fetches the value for the given key if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if an object with the `key` does not exist, cannot be accessed,
    /// or cannot be deserialized.
    #[tracing::instrument(
        name = "ObjectStore::get",
        skip_all,
        fields(key) // Will be recorded within the function.
    )]
    pub async fn get<V: StoredObject>(&self, key: V::Key<'_>) -> Result<V, ObjectStoreError> {
        let encoded_key = V::encode_key(key);
        // Record the key for tracing.
        tracing::Span::current().record("key", encoded_key.as_str());
        let bytes = match self.get_raw(V::BUCKET, &encoded_key).await {
            Ok(bytes) => bytes,
            Err(ObjectStoreError::KeyNotFound(e)) => {
                if let Some(fallback_key) = V::fallback_key(key) {
                    self.get_raw(V::BUCKET, &fallback_key).await?
                } else {
                    return Err(ObjectStoreError::KeyNotFound(e));
                }
            }
            Err(e) => return Err(e),
        };
        V::deserialize(bytes).map_err(ObjectStoreError::Serialization)
    }

    /// Fetches the value for the given encoded key if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if an object with the `encoded_key` does not exist, cannot be accessed,
    /// or cannot be deserialized.
    #[tracing::instrument(
        name = "ObjectStore::get_by_encoded_key",
        skip_all,
        fields(key = %encoded_key)
    )]
    pub async fn get_by_encoded_key<V: StoredObject>(
        &self,
        encoded_key: String,
    ) -> Result<V, ObjectStoreError> {
        let bytes = self.get_raw(V::BUCKET, &encoded_key).await?;
        V::deserialize(bytes).map_err(ObjectStoreError::Serialization)
    }

    /// Stores the value associating it with the key. If the key already exists,
    /// the value is replaced.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the insertion / replacement operation fails.
    #[tracing::instrument(
        name = "ObjectStore::put",
        skip_all,
        fields(key) // Will be recorded within the function.
    )]
    pub async fn put<V: StoredObject>(
        &self,
        key: V::Key<'_>,
        value: &V,
    ) -> Result<String, ObjectStoreError> {
        let key = V::encode_key(key);
        // Record the key for tracing.
        tracing::Span::current().record("key", key.as_str());
        let bytes = value.serialize().map_err(ObjectStoreError::Serialization)?;
        self.put_raw(V::BUCKET, &key, bytes).await?;
        Ok(key)
    }

    /// Removes a value associated with the key.
    ///
    /// # Errors
    ///
    /// Returns I/O errors specific to the storage.
    #[tracing::instrument(
        name = "ObjectStore::put",
        skip_all,
        fields(key) // Will be recorded within the function.
    )]
    pub async fn remove<V: StoredObject>(&self, key: V::Key<'_>) -> Result<(), ObjectStoreError> {
        let key = V::encode_key(key);
        // Record the key for tracing.
        tracing::Span::current().record("key", key.as_str());
        self.remove_raw(V::BUCKET, &key).await
    }

    pub fn get_storage_prefix<V: StoredObject>(&self) -> String {
        self.storage_prefix_raw(V::BUCKET)
    }
}
