use crate::config::ProofStorageConfig;
use crate::prover_api::fri_job_manager::FailedFriProof;
use crate::prover_api::metrics::{PROOF_STORAGE_METRICS, ProofStorageMethod};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::Mutex;
use zisk_prover_lane::{AggregationInput, CompletedAggregatedProof, ZiskAggregationPersistence};
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};

/// Persists FRI proofs to disk together with the batch if proof is successful.
///
/// The two `zisk_*` sections give the ZiSK aggregation lane the same restart
/// durability. The server fills them only when the second proof system is on
/// (`enable_zisk`), so the default configuration writes no ZiSK files. See the
/// `save_zisk_*` / `load_zisk_*` / `remove_zisk_*` methods.
#[derive(Clone, Debug)]
pub struct ProofStorage {
    batches_with_proof: Arc<Mutex<BoundedFileStorage>>,
    failed: Arc<Mutex<BoundedFileStorage>>,
    /// Buffered per-batch ZiSK aggregation inputs (the `vadcop_final` streams
    /// the aggregation lane collects), keyed by batch number.
    zisk_aggregation_inputs: Option<Arc<Mutex<BoundedFileStorage>>>,
    /// Parked aggregated ZiSK range proofs, keyed by range.
    zisk_aggregated_proofs: Option<Arc<Mutex<BoundedFileStorage>>>,
}
impl ProofStorage {
    pub async fn new(config: ProofStorageConfig, enable_zisk: bool) -> anyhow::Result<Self> {
        tracing::info!(
            path = config.path.display().to_string(),
            batch_with_proof_capacity = config.batch_with_proof_capacity.0,
            failed_capacity = config.failed_capacity.0,
            enable_zisk,
            "Initializing proof storage"
        );
        // The ZiSK sections are created only when the second proof system is
        // enabled, so the default configuration writes no ZiSK files.
        let (zisk_aggregation_inputs, zisk_aggregated_proofs) = if enable_zisk {
            let zisk_dir = config.path.join("zisk_proofs");
            (
                Some(Arc::new(Mutex::new(
                    BoundedFileStorage::new(
                        zisk_dir.join("aggregation_inputs"),
                        config.zisk_aggregation_input_capacity.0,
                    )
                    .await?,
                ))),
                Some(Arc::new(Mutex::new(
                    BoundedFileStorage::new(
                        zisk_dir.join("aggregated_proofs"),
                        config.zisk_aggregated_proof_capacity.0,
                    )
                    .await?,
                ))),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            batches_with_proof: Arc::new(Mutex::new(
                BoundedFileStorage::new(
                    config.path.join("fri_batches"),
                    config.batch_with_proof_capacity.0,
                )
                .await?,
            )),
            failed: Arc::new(Mutex::new(
                BoundedFileStorage::new(
                    config.path.join("failed_proofs"),
                    config.failed_capacity.0,
                )
                .await?,
            )),
            zisk_aggregation_inputs,
            zisk_aggregated_proofs,
        })
    }

    /// Persist a BatchWithProof. Overwrites any existing entry for the same batch.
    pub async fn save_batch_with_proof(&self, batch: &StoredBatch) -> anyhow::Result<()> {
        let latency =
            PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::SaveBatchWithProof].start();

        let key = format!("batch_{}.json", batch.batch_number());
        let result = self
            .batches_with_proof
            .lock()
            .await
            .store(&key, batch)
            .await;
        latency.observe();
        let usage = result?;

        PROOF_STORAGE_METRICS.disk_usage[&ProofStorageMethod::SaveBatchWithProof].set(usage);
        Ok(())
    }

    /// Loads a BatchWithProof for `batch_number`, if present
    pub async fn get_batch_with_proof(
        &self,
        batch_num: u64,
    ) -> anyhow::Result<Option<SignedBatchEnvelope<FriProof>>> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::GetBatchWithProof].start();

        let key = format!("batch_{batch_num}.json");
        let result = self
            .batches_with_proof
            .lock()
            .await
            .load::<StoredBatch>(&key)
            .await
            .map(|o| o.map(|o| o.batch_envelope()));

        latency.observe();
        result
    }

    /// Save a failed FRI proof for debugging.
    pub async fn save_failed_proof(&self, proof: &FailedFriProof) -> anyhow::Result<()> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::SaveFailed].start();

        let key = format!("failed_{}.json", proof.batch_number);
        let result = self.failed.lock().await.store(&key, proof).await;
        latency.observe();
        let usage = result?;

        PROOF_STORAGE_METRICS.disk_usage[&ProofStorageMethod::SaveFailed].set(usage);
        Ok(())
    }

    /// Get the failed proof for a given batch number.
    /// Returns None if no failed proof exists for this batch.
    pub async fn get_failed_proof(&self, batch_num: u64) -> anyhow::Result<Option<FailedFriProof>> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::GetFailed].start();

        let key = format!("failed_{batch_num}.json");
        let result = self.failed.lock().await.load(&key).await;

        latency.observe();
        result
    }

    // ---- ZiSK aggregation inputs (`ZiskAggregationJobManager` inputs) ----
    //
    // Both aggregation methods are no-ops (save/remove/prune) or return empty
    // (load) when the ZiSK sections are absent, i.e. when the second proof
    // system is off. The aggregation manager only calls them once it holds this
    // store, and it holds it only when the second proof system is on.

    /// Persist a buffered per-batch ZiSK aggregation input.
    pub async fn save_zisk_aggregation_input(
        &self,
        batch_number: u64,
        input: &AggregationInput,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.zisk_aggregation_inputs else {
            return Ok(());
        };
        let record = StoredZiskAggregationInput::V1(input.clone());
        store
            .lock()
            .await
            .store(&batch_key(batch_number), &record)
            .await?;
        Ok(())
    }

    /// Load every buffered ZiSK aggregation input still on disk.
    pub async fn load_zisk_aggregation_inputs(
        &self,
    ) -> anyhow::Result<Vec<(u64, AggregationInput)>> {
        let Some(store) = &self.zisk_aggregation_inputs else {
            return Ok(Vec::new());
        };
        let store = store.lock().await;
        let mut out = Vec::new();
        for key in store.keys().await? {
            let Some(batch_number) = parse_batch_key(&key) else {
                continue;
            };
            match store.load::<StoredZiskAggregationInput>(&key).await {
                Ok(Some(StoredZiskAggregationInput::V1(input))) => out.push((batch_number, input)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%key, "skipping unreadable persisted ZiSK aggregation input: {e}")
                }
            }
        }
        Ok(out)
    }

    // ---- ZiSK aggregated range proofs (`ZiskAggregationJobManager` completed) ----

    /// Persist a parked aggregated ZiSK range proof.
    pub async fn save_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: &CompletedAggregatedProof,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.zisk_aggregated_proofs else {
            return Ok(());
        };
        let record = StoredZiskAggregatedProof::V1(proof.clone());
        store
            .lock()
            .await
            .store(&range_key(from_batch, to_batch), &record)
            .await?;
        Ok(())
    }

    /// Remove the parked aggregated ZiSK range proof for `from_batch..=to_batch`.
    pub async fn remove_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.zisk_aggregated_proofs else {
            return Ok(());
        };
        store
            .lock()
            .await
            .remove(&range_key(from_batch, to_batch))
            .await
    }

    /// Load every parked aggregated ZiSK range proof still on disk.
    pub async fn load_zisk_aggregated_proofs(
        &self,
    ) -> anyhow::Result<Vec<((u64, u64), CompletedAggregatedProof)>> {
        let Some(store) = &self.zisk_aggregated_proofs else {
            return Ok(Vec::new());
        };
        let store = store.lock().await;
        let mut out = Vec::new();
        for key in store.keys().await? {
            let Some(range) = parse_range_key(&key) else {
                continue;
            };
            match store.load::<StoredZiskAggregatedProof>(&key).await {
                Ok(Some(StoredZiskAggregatedProof::V1(proof))) => out.push((range, proof)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%key, "skipping unreadable persisted aggregated ZiSK proof: {e}")
                }
            }
        }
        Ok(out)
    }

    /// Drop persisted ZiSK artifacts for batches at or below `batch_to`. Mirrors
    /// the in-memory retirement the managers do when a batch is settled or sent
    /// downstream: per-batch proofs and aggregation inputs at or below the cut,
    /// and aggregated ranges that start at or below it (the predicate
    /// `State::retire_up_to` uses).
    pub async fn prune_zisk_up_to(&self, batch_to: u64) -> anyhow::Result<()> {
        if let Some(store) = &self.zisk_aggregation_inputs {
            let mut store = store.lock().await;
            for key in store.keys().await? {
                if let Some(batch) = parse_batch_key(&key)
                    && batch <= batch_to
                {
                    store.remove(&key).await?;
                }
            }
        }
        if let Some(store) = &self.zisk_aggregated_proofs {
            let mut store = store.lock().await;
            for key in store.keys().await? {
                if let Some((from, _to)) = parse_range_key(&key)
                    && from <= batch_to
                {
                    store.remove(&key).await?;
                }
            }
        }
        Ok(())
    }
}

/// The ZiSK aggregation lane writes through this store via the crate's
/// persistence trait. Each method forwards to the inherent `save_zisk_*` /
/// `remove_zisk_*` / `prune_zisk_*` methods above, which are no-ops when the
/// ZiSK sections are absent (the second proof system is off).
#[async_trait::async_trait]
impl ZiskAggregationPersistence for ProofStorage {
    async fn save_zisk_aggregation_input(
        &self,
        batch_number: u64,
        input: &AggregationInput,
    ) -> anyhow::Result<()> {
        self.save_zisk_aggregation_input(batch_number, input).await
    }

    async fn save_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: &CompletedAggregatedProof,
    ) -> anyhow::Result<()> {
        self.save_zisk_aggregated_proof(from_batch, to_batch, proof)
            .await
    }

    async fn remove_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
    ) -> anyhow::Result<()> {
        self.remove_zisk_aggregated_proof(from_batch, to_batch)
            .await
    }

    async fn prune_zisk_up_to(&self, batch_to: u64) -> anyhow::Result<()> {
        self.prune_zisk_up_to(batch_to).await
    }
}

fn batch_key(batch_number: u64) -> String {
    format!("batch_{batch_number}.json")
}

fn parse_batch_key(key: &str) -> Option<u64> {
    key.strip_prefix("batch_")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

fn range_key(from_batch: u64, to_batch: u64) -> String {
    format!("range_{from_batch}_{to_batch}.json")
}

fn parse_range_key(key: &str) -> Option<(u64, u64)> {
    let body = key.strip_prefix("range_")?.strip_suffix(".json")?;
    let (from, to) = body.split_once('_')?;
    Some((from.parse().ok()?, to.parse().ok()?))
}

/// Persisted form of a buffered per-batch ZiSK aggregation input.
#[derive(Serialize, Deserialize)]
#[non_exhaustive]
enum StoredZiskAggregationInput {
    V1(AggregationInput),
}

/// Persisted form of a parked aggregated ZiSK range proof.
#[derive(Serialize, Deserialize)]
#[non_exhaustive]
enum StoredZiskAggregatedProof {
    V1(CompletedAggregatedProof),
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StoredBatch {
    V1(SignedBatchEnvelope<FriProof>),
}

impl StoredBatch {
    pub fn batch_number(&self) -> u64 {
        match self {
            StoredBatch::V1(envelope) => envelope.batch_number(),
        }
    }

    pub fn batch_envelope(self) -> SignedBatchEnvelope<FriProof> {
        match self {
            StoredBatch::V1(envelope) => envelope,
        }
    }
}

/// Storage for data blobs that
/// automatically removes old files to keep disk usage within capacity_bytes
/// Keys are expected to be file names.
/// In case of overwrite old value will be preserved under a different name (see handle_duplicate)
/// Expected use case for this data is debugging.
/// The only way to access overwritten entries is directly from disk.
/// Currently, the key is batch number. Overwrites could happen in these 2 cases:
/// * server restart -- we do not store block ranges for the batches, so they could change
/// * batch revert
#[derive(Clone, Debug)]
pub(crate) struct BoundedFileStorage {
    base_dir: PathBuf,
    capacity_bytes: u64,
    current_size: u64,
    /// Files ordered by eviction priority (oldest first). New files are pushed to the back;
    /// eviction pops from the front.
    ///
    /// A key may appear more than once when a file has been overwritten: the original queue
    /// entry becomes outdated (the file was renamed away) while the renamed file and the new
    /// file each add their own entry. Outdated entries must be skipped during eviction — see
    /// `outdated_count`.
    remove_queue: VecDeque<(String, Metadata)>,
    /// Counts outdated entries in `remove_queue` for each key.
    ///
    /// Each time a key is overwritten, `handle_duplicate` renames the existing file and
    /// increments this counter. The original queue entry (still carrying the old key) becomes
    /// outdated: the file it pointed to no longer exists under that name. During eviction,
    /// `enforce_capacity` decrements the counter and skips the entry instead of trying to
    /// delete it, preventing accidental deletion of the current version of the file.
    outdated_count: HashMap<String, u64>,
}

impl BoundedFileStorage {
    pub(crate) async fn new(base_dir: PathBuf, capacity_bytes: u64) -> anyhow::Result<Self> {
        // Create the directory if it doesn't exist already
        fs::create_dir_all(&base_dir).await?;
        // List all files sorted by timestamp (descending)
        let mut entries = fs::read_dir(&base_dir).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_file() {
                match entry.file_name().into_string() {
                    Ok(filename) => files.push((filename, meta)),
                    Err(filename) => tracing::warn!(
                        "Unrelated file detected in {} ({}): the name cannot be represented using a String",
                        base_dir.display(),
                        filename.to_string_lossy(),
                    ),
                }
            }
        }
        files.sort_by_cached_key(|(_, meta)| meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));

        let current_size: u64 = files.iter().map(|(_, meta)| meta.len()).sum();
        let mut storage = Self {
            base_dir,
            capacity_bytes,
            current_size,
            remove_queue: files.into_iter().collect(),
            outdated_count: HashMap::new(),
        };

        if current_size > capacity_bytes {
            tracing::warn!(
                current_size,
                capacity_bytes,
                "On startup, more data is used than expected"
            );
            storage.enforce_capacity(0).await?;
        }

        Ok(storage)
    }

    /// Stores serialized value as a file named `key` (should be a valid file name)
    /// Previous `value` for `key` is preserved under a different name, with a recent timestamp
    /// removes old files to enforce capacity constraints and
    /// returns disk usage
    pub(crate) async fn store<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
    ) -> anyhow::Result<u64> {
        fs::create_dir_all(&self.base_dir).await?;

        let data = serde_json::to_vec(value)?;
        let count = data.len() as u64;
        self.handle_duplicate(key).await?;
        // This could still remove the duplicate if there is not enough space for it
        self.enforce_capacity(count).await?;
        if count <= self.capacity_bytes {
            self.write_file(key, data).await?;
        } else {
            tracing::warn!(
                data_len = data.len(),
                capacity = self.capacity_bytes,
                "Entry size is larger than the limit. Not saving.",
            );
        }
        Ok(self.current_size)
    }

    pub(crate) async fn load<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let path = self.base_dir.join(key);
        if !fs::try_exists(&path).await? {
            return Ok(None);
        }

        let data = fs::read(path).await?;
        let decoded = serde_json::from_slice(&data)?;
        Ok(Some(decoded))
    }

    /// List the keys (file names) of the current live entries. Superseded
    /// copies renamed by `handle_duplicate` (`*.overwritten_*`) are skipped.
    /// Reads the directory, so callers use it for the rare reload and prune
    /// paths, not on every store.
    pub(crate) async fn keys(&self) -> anyhow::Result<Vec<String>> {
        let mut entries = fs::read_dir(&self.base_dir).await?;
        let mut keys = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.metadata().await?.is_file()
                && let Ok(name) = entry.file_name().into_string()
                && !name.contains(".overwritten_")
            {
                keys.push(name);
            }
        }
        Ok(keys)
    }

    /// Delete the live file for `key` and free the space it used. A missing
    /// file is a no-op.
    ///
    /// The queue entry for `key` is left in place but marked outdated (the
    /// same `outdated_count` mechanism `handle_duplicate` uses): `enforce_capacity`
    /// then skips it instead of deleting the already-removed file. This stays
    /// correct across a later re-store of the same key. A re-store pushes a
    /// fresh entry to the BACK of the queue, after the outdated entry, so
    /// eviction consumes the outdated skip first and reaches the live entry
    /// only afterwards.
    pub(crate) async fn remove(&mut self, key: &str) -> anyhow::Result<()> {
        let path = self.base_dir.join(key);
        if !fs::try_exists(&path).await? {
            return Ok(());
        }
        let meta = fs::metadata(&path).await?;
        fs::remove_file(&path).await?;
        self.current_size = self.current_size.saturating_sub(meta.len());
        *self.outdated_count.entry(key.to_string()).or_insert(0) += 1;
        Ok(())
    }

    /// Delete old files to make space for the new file
    async fn enforce_capacity(&mut self, new_file_size: u64) -> anyhow::Result<()> {
        // Delete old files to satisfy capacity constraints
        while self.current_size + new_file_size > self.capacity_bytes
            && !self.remove_queue.is_empty()
        {
            let (key, meta) = self.remove_queue.pop_front().unwrap();
            // This queue entry is outdated: the file was renamed away by a later overwrite.
            // Skip it without touching the filesystem and decrement the counter.
            // The renamed file is tracked separately under its new name.
            if let Some(outdated) = self.outdated_count.get_mut(&key)
                && *outdated > 0
            {
                *outdated -= 1;
                continue;
            }

            fs::remove_file(self.base_dir.join(key)).await?;
            self.current_size -= meta.len();
        }

        if self.remove_queue.is_empty() && self.current_size > 0 {
            tracing::warn!(
                current_size = self.current_size,
                "current_size is not maintained correctly"
            );
        }

        Ok(())
    }
    /// If a file named `key` already exists, renames it to `key.overwritten_{timestamp}`
    /// and appends the renamed entry to the back of the queue so it is eventually evicted.
    async fn handle_duplicate(&mut self, key: &str) -> anyhow::Result<()> {
        let path = self.base_dir.join(key);
        if path.is_file() {
            tracing::info!("Storing old version of {}", key);

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let new_key = &format!("{key}.overwritten_{now}");
            let new_path = self.base_dir.join(new_key);
            // The original queue entry for `key` becomes outdated: the file it pointed to
            // no longer exists under that name. Increment the counter so that
            // `enforce_capacity` knows to skip that entry rather than deleting the
            // newly-written file.
            *self.outdated_count.entry(key.to_string()).or_insert(0) += 1;
            // Rename and add to the back of the queue
            fs::rename(path, new_path.clone()).await?;
            let meta = fs::metadata(&new_path).await?;
            self.remove_queue.push_back((new_key.to_string(), meta));
        }
        Ok(())
    }

    /// Write file to disk and add an entry to remove_queue
    async fn write_file(&mut self, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.base_dir.join(key);
        let len = data.len() as u64;
        fs::write(&path, data).await?;
        self.current_size += len;
        let meta = fs::metadata(&path).await?;
        self.remove_queue.push_back((key.to_string(), meta));
        Ok(())
    }
}

// Since this data isn't used by the node itself, I added some tests
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zksync_os_types::ProtocolSemanticVersion;

    // Make sure files are being removed as expected
    #[tokio::test]
    async fn test_bounded_storage_capacity() -> anyhow::Result<()> {
        const LIMIT: u64 = 20000;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;

        // Many small files
        let num_iter = 2000;
        for i in 0..num_iter {
            let key: String = i.to_string();
            let val = "a".repeat((LIMIT / num_iter) as usize);
            storage.store(&key, &val).await?;
            assert_eq!(storage.load::<String>(key.as_str()).await?, Some(val));
            if i >= num_iter {
                assert!(
                    storage
                        .load::<String>(&(i - num_iter + 1).to_string())
                        .await?
                        .is_some()
                );
                assert!(
                    storage
                        .load::<String>(&(i - num_iter).to_string())
                        .await?
                        .is_none()
                );
            }
        }

        // Large files
        let big_str = "a".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str).await?;
        // This removes most entries but not all
        assert!(
            storage
                .load::<String>(&(num_iter / 2).to_string())
                .await?
                .is_none()
        );
        assert!(
            storage
                .load::<String>(&(num_iter - 1).to_string())
                .await?
                .is_some()
        );
        // This should remove all the old entries
        storage.store("key2", &big_str).await?;
        assert!(storage.load::<String>("key").await?.is_none());
        // Files larger than limit won't be stored
        let very_big = "a".repeat((2 * LIMIT) as usize);
        storage.store("key", &very_big).await?;
        assert!(storage.load::<String>("key").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_bounded_storage_overwrites() -> anyhow::Result<()> {
        const LIMIT: u64 = 1 << 20;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;
        // overrides in case of large strings
        let big_str_a = "a".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_a).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_a));
        let big_str_b = "b".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_b).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_b));
        Ok(())
    }

    #[tokio::test]
    async fn test_bounded_storage_overwrite_cleanup() -> anyhow::Result<()> {
        const LIMIT: u64 = 506;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;

        let str1 = "a".repeat(100);
        let str2 = "ab".repeat(100);
        storage.store("0", &str2).await?;
        storage.store("1", &str2).await?;
        storage.store("0", &str1).await?;
        // TODO: handle acse when overwrite is the same value
        storage.store("0", &str2).await?;
        assert_eq!(storage.load::<String>("1").await?, None);
        storage.store("1", &str2).await?;
        // Duplicate was removed here
        assert!(storage.load::<String>("0").await?.is_some());
        assert!(storage.load::<String>("1").await?.is_some());

        Ok(())
    }

    // `remove` deletes the live file and keeps eviction correct across a
    // later re-store of the same key.
    #[tokio::test]
    async fn test_bounded_storage_remove_and_restore() -> anyhow::Result<()> {
        const LIMIT: u64 = 1 << 20;
        let dir = TempDir::new()?;
        let mut storage = BoundedFileStorage::new(dir.path().to_owned(), LIMIT).await?;

        storage.store("k", &"a".repeat(100)).await?;
        storage.remove("k").await?;
        assert_eq!(storage.load::<String>("k").await?, None);
        assert!(!storage.keys().await?.contains(&"k".to_string()));

        // A store after a remove writes a fresh live entry that is NOT skipped
        // by the outdated bookkeeping `remove` left behind. A small capacity
        // forces eviction to exercise that queue interaction.
        let mut tight = BoundedFileStorage::new(dir.path().join("tight"), 250).await?;
        tight.store("k", &"a".repeat(100)).await?;
        tight.remove("k").await?;
        tight.store("k", &"b".repeat(100)).await?;
        tight.store("other", &"c".repeat(100)).await?;
        assert_eq!(tight.load::<String>("k").await?, Some("b".repeat(100)));
        Ok(())
    }

    fn zisk_config(dir: &TempDir) -> ProofStorageConfig {
        use smart_config::ByteSize;
        ProofStorageConfig {
            path: dir.path().to_owned(),
            batch_with_proof_capacity: ByteSize(1 << 30),
            failed_capacity: ByteSize(1 << 30),
            zisk_aggregation_input_capacity: ByteSize(1 << 30),
            zisk_aggregated_proof_capacity: ByteSize(1 << 30),
        }
    }

    /// Aggregation inputs and aggregated range proofs round-trip through save
    /// and reload, and `prune_zisk_up_to` drops everything at or below the cut.
    #[tokio::test]
    async fn zisk_aggregation_state_survives_and_prunes() -> anyhow::Result<()> {
        use alloy::primitives::B256;
        let dir = TempDir::new()?;
        let storage = ProofStorage::new(zisk_config(&dir), true).await?;

        let input = AggregationInput {
            stream: vec![3u8; 2048],
            protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
            program_vk: B256::repeat_byte(0x11),
            vadcop_vk: B256::repeat_byte(0x22),
            commitment: B256::repeat_byte(0x33),
        };
        storage.save_zisk_aggregation_input(5, &input).await?;
        storage.save_zisk_aggregation_input(6, &input).await?;

        let range_proof = CompletedAggregatedProof {
            proof: vec![4u8; 768],
            public_values: vec![5u8; 320],
        };
        storage
            .save_zisk_aggregated_proof(5, 6, &range_proof)
            .await?;

        // Prune everything at or below batch 5: input 5 goes, input 6 stays,
        // and the range 5..=6 goes because it starts at 5.
        storage.prune_zisk_up_to(5).await?;

        let reloaded = ProofStorage::new(zisk_config(&dir), true).await?;
        let input_batches: Vec<u64> = reloaded
            .load_zisk_aggregation_inputs()
            .await?
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        assert_eq!(input_batches, vec![6]);
        assert!(reloaded.load_zisk_aggregated_proofs().await?.is_empty());
        Ok(())
    }

    /// With the second proof system off, the ZiSK sections are absent: saves are
    /// no-ops, loads are empty, and no ZiSK directory is written.
    #[tokio::test]
    async fn zisk_disabled_writes_nothing() -> anyhow::Result<()> {
        use alloy::primitives::B256;
        let dir = TempDir::new()?;
        let storage = ProofStorage::new(zisk_config(&dir), false).await?;
        let input = AggregationInput {
            stream: vec![0u8; 2048],
            protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
            program_vk: B256::repeat_byte(0x11),
            vadcop_vk: B256::repeat_byte(0x22),
            commitment: B256::repeat_byte(0x33),
        };
        storage.save_zisk_aggregation_input(1, &input).await?;
        assert!(storage.load_zisk_aggregation_inputs().await?.is_empty());
        assert!(!dir.path().join("zisk_proofs").exists());
        Ok(())
    }

    /// First boot after an upgrade: the `zisk_proofs/` directory does not exist
    /// yet. The store creates it lazily and every reload finds nothing, with no
    /// error and no migration step. This keeps the change migration-free.
    #[tokio::test]
    async fn zisk_first_boot_over_empty_dir_reloads_nothing() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        assert!(!dir.path().join("zisk_proofs").exists());

        let storage = ProofStorage::new(zisk_config(&dir), true).await?;
        // The reload paths must succeed and return empty on a fresh directory.
        assert!(storage.load_zisk_aggregation_inputs().await?.is_empty());
        assert!(storage.load_zisk_aggregated_proofs().await?.is_empty());
        // Pruning an empty store is a no-op, not an error.
        storage.prune_zisk_up_to(1000).await?;
        Ok(())
    }
}
