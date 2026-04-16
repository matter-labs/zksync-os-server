use super::{RepositoryCF, RepositoryDb};
use alloy::primitives::{Address, B256};
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::io::Cursor;
use std::ops::Range;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::WriteBatch;
use zksync_os_storage_api::{LogIndex, RepositoryResult};

/// Chunk size for log index bitmaps. Equals 2^16 = 65536, which aligns with roaring32's
/// internal container boundary: all block numbers within a chunk share the same high 16 bits,
/// so each chunk serializes to exactly one roaring container (max 8 KB).
pub(super) const CHUNK_SIZE: u64 = 1 << 16;

pub(super) fn chunk_start(block_number: u64) -> u64 {
    // Round down to the nearest CHUNK_SIZE boundary (equivalent to
    // `(block_number / CHUNK_SIZE) * CHUNK_SIZE` but without a division).
    block_number & !(CHUNK_SIZE - 1)
}

pub(super) fn address_chunk_key(address: Address, chunk_start: u64) -> [u8; 28] {
    let mut key = [0u8; 28];
    key[..20].copy_from_slice(address.as_slice());
    key[20..].copy_from_slice(&chunk_start.to_be_bytes());
    key
}

pub(super) fn topic_chunk_key(topic: B256, chunk_start: u64) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(topic.as_slice());
    key[32..].copy_from_slice(&chunk_start.to_be_bytes());
    key
}

fn serialize_bitmap(bitmap: &RoaringBitmap) -> Vec<u8> {
    let mut buf = Vec::with_capacity(bitmap.serialized_size());
    bitmap
        .serialize_into(&mut buf)
        .expect("Vec<u8> write cannot fail");
    buf
}

fn deserialize_bitmap(bytes: &[u8]) -> RoaringBitmap {
    RoaringBitmap::deserialize_from(Cursor::new(bytes)).expect("log index bitmap is corrupted")
}

/// In-memory cache of pending bitmap mutations for a single write batch.
/// Reads fall through to the DB on first access; writes stay in the cache until
/// `flush` copies them to the batch.  Using a cache means that multiple mutations
/// to the same key within one batch compose correctly — without it, each read
/// would see the pre-batch DB state and later writes would silently overwrite
/// earlier ones.
#[derive(Default)]
pub(super) struct BitmapCache {
    by_address: HashMap<Vec<u8>, RoaringBitmap>,
    by_topic: HashMap<Vec<u8>, RoaringBitmap>,
}

impl BitmapCache {
    /// Writes all cached bitmap mutations to `batch`, consuming the cache.
    pub(super) fn flush(self, batch: &mut WriteBatch<RepositoryCF>) {
        write_bitmaps_to_batch(self.by_address, RepositoryCF::LogBlocksByAddress, batch);
        write_bitmaps_to_batch(self.by_topic, RepositoryCF::LogBlocksByTopic, batch);
    }
}

fn write_bitmaps_to_batch(
    bitmaps: HashMap<Vec<u8>, RoaringBitmap>,
    cf: RepositoryCF,
    batch: &mut WriteBatch<RepositoryCF>,
) {
    for (key, bitmap) in bitmaps {
        if bitmap.is_empty() {
            batch.delete_cf(cf, &key);
        } else {
            batch.put_cf(cf, &key, &serialize_bitmap(&bitmap));
        }
    }
}

/// Mutates the bitmap for `key` in `bitmaps`, loading it from `db` on first access.
fn with_chunk(
    db: &RocksDB<RepositoryCF>,
    bitmaps: &mut HashMap<Vec<u8>, RoaringBitmap>,
    cf: RepositoryCF,
    key: &[u8],
    f: impl FnOnce(&mut RoaringBitmap),
) -> RepositoryResult<()> {
    let bitmap = if let Some(bm) = bitmaps.get_mut(key) {
        bm
    } else {
        let bm = match db.get_cf(cf, key)? {
            Some(bytes) => deserialize_bitmap(&bytes),
            None => RoaringBitmap::new(),
        };
        bitmaps.entry(key.to_vec()).or_insert(bm)
    };
    f(bitmap);
    Ok(())
}

/// Updates the log index coverage metadata in `batch`.
/// Writes `log_index_first_block` if it is not already present in the DB
/// (it is only ever set once); always updates `log_index_last_block`.
///
/// Must be called at most once per `batch`: the first-block check reads from
/// the DB, not the batch, so a second call within the same batch would
/// overwrite the first-block key even though it was already set by the batch.
pub(super) fn update_coverage(
    db: &RocksDB<RepositoryCF>,
    batch: &mut WriteBatch<RepositoryCF>,
    block_number_bytes: &[u8],
) {
    let first_block_key = RepositoryCF::log_index_first_block_key();
    if db
        .get_cf(RepositoryCF::Meta, first_block_key)
        .unwrap()
        .is_none()
    {
        batch.put_cf(RepositoryCF::Meta, first_block_key, block_number_bytes);
    }
    batch.put_cf(
        RepositoryCF::Meta,
        RepositoryCF::log_index_last_block_key(),
        block_number_bytes,
    );
}

/// Rolls back the log index last-block marker to `block_number_bytes`.
pub(super) fn rollback_coverage(batch: &mut WriteBatch<RepositoryCF>, block_number_bytes: &[u8]) {
    batch.put_cf(
        RepositoryCF::Meta,
        RepositoryCF::log_index_last_block_key(),
        block_number_bytes,
    );
}

/// Adds all logs from a single transaction to the bitmap cache.
pub(super) fn index_logs<'a>(
    db: &RocksDB<RepositoryCF>,
    cache: &mut BitmapCache,
    block_number: u64,
    logs: impl IntoIterator<Item = &'a alloy::primitives::Log>,
) -> RepositoryResult<()> {
    let chunk = chunk_start(block_number);
    let block_offset = (block_number - chunk) as u32;
    for log in logs {
        with_chunk(
            db,
            &mut cache.by_address,
            RepositoryCF::LogBlocksByAddress,
            &address_chunk_key(log.address, chunk),
            |bm| {
                bm.insert(block_offset);
            },
        )?;
        for topic in log.topics() {
            with_chunk(
                db,
                &mut cache.by_topic,
                RepositoryCF::LogBlocksByTopic,
                &topic_chunk_key(*topic, chunk),
                |bm| {
                    bm.insert(block_offset);
                },
            )?;
        }
    }
    Ok(())
}

/// Removes all logs from a single transaction from the bitmap cache.
pub(super) fn deindex_logs<'a>(
    db: &RocksDB<RepositoryCF>,
    cache: &mut BitmapCache,
    block_number: u64,
    logs: impl IntoIterator<Item = &'a alloy::primitives::Log>,
) -> RepositoryResult<()> {
    let chunk = chunk_start(block_number);
    let block_offset = (block_number - chunk) as u32;
    for log in logs {
        with_chunk(
            db,
            &mut cache.by_address,
            RepositoryCF::LogBlocksByAddress,
            &address_chunk_key(log.address, chunk),
            |bm| {
                bm.remove(block_offset);
            },
        )?;
        for topic in log.topics() {
            with_chunk(
                db,
                &mut cache.by_topic,
                RepositoryCF::LogBlocksByTopic,
                &topic_chunk_key(*topic, chunk),
                |bm| {
                    bm.remove(block_offset);
                },
            )?;
        }
    }
    Ok(())
}

fn read_u64_meta(db: &RocksDB<RepositoryCF>, key: &[u8]) -> RepositoryResult<Option<u64>> {
    Ok(db.get_cf(RepositoryCF::Meta, key)?.map(|v: Vec<u8>| {
        u64::from_be_bytes(v.as_slice().try_into().expect("metadata must be 8 bytes"))
    }))
}

/// Returns the index coverage as a half-open range, or `None` if the index is empty.
fn coverage(db: &RocksDB<RepositoryCF>) -> RepositoryResult<Option<Range<u64>>> {
    let first = read_u64_meta(db, RepositoryCF::log_index_first_block_key())?;
    let last = read_u64_meta(db, RepositoryCF::log_index_last_block_key())?;
    Ok(first.zip(last).map(|(f, l)| f..l + 1))
}

/// Reads all chunks overlapping `range` for `key_prefix` from `cf`, ORs them together,
/// and masks to `range`.
///
/// Uses `range_iterator_cf` so that RocksDB can exploit per-prefix bloom filters
/// (configured via `prefix_extractor_len`) to skip SST files that contain no chunks for
/// this address or topic — useful when the queried address/topic is sparse.
fn read_range(
    db: &RocksDB<RepositoryCF>,
    cf: RepositoryCF,
    key_prefix: &[u8],
    range: Range<u64>,
) -> RepositoryResult<RoaringBitmap> {
    let from_chunk = chunk_start(range.start);
    // Upper bound: first key that is lexicographically past the last chunk we need.
    // chunk_start(range.end - 1) + CHUNK_SIZE is the chunk *after* the last relevant one,
    // and since chunk bytes are the suffix, appending 0xFF..FF would also work, but an
    // exact chunk key is cleaner and keeps us within the same prefix.
    let to_chunk_exclusive = chunk_start(range.end.saturating_sub(1)) + CHUNK_SIZE;

    let mut from_key = key_prefix.to_vec();
    from_key.extend_from_slice(&from_chunk.to_be_bytes());
    let mut to_key = key_prefix.to_vec();
    to_key.extend_from_slice(&to_chunk_exclusive.to_be_bytes());

    let mut result = RoaringBitmap::new();
    for (key, value) in db.range_iterator_cf(cf, from_key.as_slice()..to_key.as_slice()) {
        // Bitmaps store offsets relative to chunk_start; we need to recover the chunk base
        // from the key suffix to shift offsets back to absolute block numbers.
        let chunk_bytes: [u8; 8] = key[key_prefix.len()..]
            .try_into()
            .expect("chunk key suffix must be 8 bytes");
        let chunk_base = u64::from_be_bytes(chunk_bytes) as u32;
        result |= deserialize_bitmap(&value)
            .into_iter()
            .map(|offset| chunk_base + offset)
            .collect::<RoaringBitmap>();
    }

    // Trim block numbers outside the requested range that leaked in from
    // partially-overlapping boundary chunks. `remove_range` runs in
    // O(containers removed) time rather than O(bits), so this is efficient
    // even when the trimmed region is large.
    result.remove_range(..range.start as u32);
    result.remove_range(range.end as u32..);
    Ok(result)
}

/// Intersects `range` with `coverage` and, if non-empty, reads the bitmap for `key_prefix`.
pub(super) fn query(
    db: &RocksDB<RepositoryCF>,
    cf: RepositoryCF,
    key_prefix: &[u8],
    range: Range<u64>,
) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
    let Some(cov) = coverage(db)? else {
        return Ok((RoaringBitmap::new(), 0..0));
    };
    let covered = range.start.max(cov.start)..range.end.min(cov.end);
    if covered.is_empty() {
        return Ok((RoaringBitmap::new(), 0..0));
    }
    let bitmap = read_range(db, cf, key_prefix, covered.clone())?;
    Ok((bitmap, covered))
}

impl LogIndex for RepositoryDb {
    fn blocks_for_address(
        &self,
        address: Address,
        range: Range<u64>,
    ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
        query(
            &self.db,
            RepositoryCF::LogBlocksByAddress,
            address.as_slice(),
            range,
        )
    }

    fn blocks_for_topic(
        &self,
        topic: B256,
        range: Range<u64>,
    ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
        query(
            &self.db,
            RepositoryCF::LogBlocksByTopic,
            topic.as_slice(),
            range,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::LogData;
    use tempfile::TempDir;

    fn open_test_db() -> (RocksDB<RepositoryCF>, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = RocksDB::<RepositoryCF>::new(dir.path()).unwrap();
        (db, dir)
    }

    fn make_log(address: Address, topics: &[B256]) -> alloy::primitives::Log {
        alloy::primitives::Log {
            address,
            data: LogData::new_unchecked(topics.to_vec(), Default::default()),
        }
    }

    /// Writes log index entries and coverage for `block_number` using the production functions.
    fn index_block(db: &RocksDB<RepositoryCF>, block_number: u64, logs: &[alloy::primitives::Log]) {
        let mut batch = db.new_write_batch();
        let mut cache = BitmapCache::default();
        index_logs(db, &mut cache, block_number, logs).unwrap();
        cache.flush(&mut batch);
        update_coverage(db, &mut batch, &block_number.to_be_bytes());
        db.write(batch).unwrap();
    }

    #[test]
    fn address_index_round_trip() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);
        let log = make_log(addr, &[]);

        index_block(&db, 0, std::slice::from_ref(&log));
        index_block(&db, 1, &[log]);

        let (bitmap, covered) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr.as_slice(),
            0..10,
        )
        .unwrap();
        assert_eq!(covered, 0..2);
        assert!(bitmap.contains(0));
        assert!(bitmap.contains(1));
    }

    #[test]
    fn topic_index_round_trip() {
        let (db, _dir) = open_test_db();
        let topic = B256::repeat_byte(1);
        let log = make_log(Address::ZERO, &[topic]);

        index_block(&db, 0, &[log]);

        let (bitmap, covered) =
            query(&db, RepositoryCF::LogBlocksByTopic, topic.as_slice(), 0..10).unwrap();
        assert_eq!(covered, 0..1);
        assert!(bitmap.contains(0));
    }

    #[test]
    fn query_respects_requested_range() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);
        let log = make_log(addr, &[]);

        index_block(&db, 0, std::slice::from_ref(&log));
        index_block(&db, 1, std::slice::from_ref(&log));
        index_block(&db, 2, &[log]);

        // Request only blocks 1..=2 — block 0 must not appear.
        let (bitmap, covered) =
            query(&db, RepositoryCF::LogBlocksByAddress, addr.as_slice(), 1..3).unwrap();
        assert_eq!(covered, 1..3);
        assert!(!bitmap.contains(0));
        assert!(bitmap.contains(1));
        assert!(bitmap.contains(2));
    }

    #[test]
    fn deindex_removes_block() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);
        let log = make_log(addr, &[]);

        index_block(&db, 0, std::slice::from_ref(&log));
        index_block(&db, 1, std::slice::from_ref(&log));

        let mut batch = db.new_write_batch();
        let mut cache = BitmapCache::default();
        deindex_logs(&db, &mut cache, 1, &[log]).unwrap();
        cache.flush(&mut batch);
        rollback_coverage(&mut batch, &0u64.to_be_bytes());
        db.write(batch).unwrap();

        let (bitmap, covered) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr.as_slice(),
            0..10,
        )
        .unwrap();
        assert_eq!(covered, 0..1);
        assert!(bitmap.contains(0));
        assert!(!bitmap.contains(1));
    }

    #[test]
    fn no_index_returns_empty() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);

        let (bitmap, covered) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr.as_slice(),
            0..10,
        )
        .unwrap();
        assert!(covered.is_empty());
        assert!(bitmap.is_empty());
    }

    #[test]
    fn different_addresses_do_not_interfere() {
        let (db, _dir) = open_test_db();
        let addr_a = Address::repeat_byte(1);
        let addr_b = Address::repeat_byte(2);

        index_block(&db, 0, &[make_log(addr_a, &[])]);
        index_block(&db, 1, &[make_log(addr_b, &[])]);

        let (bitmap_a, _) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr_a.as_slice(),
            0..10,
        )
        .unwrap();
        let (bitmap_b, _) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr_b.as_slice(),
            0..10,
        )
        .unwrap();

        assert!(bitmap_a.contains(0) && !bitmap_a.contains(1));
        assert!(bitmap_b.contains(1) && !bitmap_b.contains(0));
    }

    #[test]
    fn chunk_boundary_spanning_query() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);
        let log = make_log(addr, &[]);

        let last_in_chunk0 = CHUNK_SIZE - 1;
        let first_in_chunk1 = CHUNK_SIZE;
        index_block(&db, last_in_chunk0, std::slice::from_ref(&log));
        index_block(&db, first_in_chunk1, &[log]);

        let (bitmap, covered) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr.as_slice(),
            0..CHUNK_SIZE + 1,
        )
        .unwrap();
        assert!(bitmap.contains(last_in_chunk0 as u32));
        assert!(bitmap.contains(first_in_chunk1 as u32));
        assert_eq!(covered, last_in_chunk0..first_in_chunk1 + 1);
    }

    /// Regression test: rolling back multiple consecutive blocks that all emitted logs for the
    /// same address must remove *all* of them, not just the last one.
    ///
    /// The old `deindex_logs` read bitmaps directly from the DB on every call, so within one
    /// write batch the last call silently overwrote the earlier ones, leaving stale entries.
    #[test]
    fn multi_block_rollback_removes_all_blocks() {
        let (db, _dir) = open_test_db();
        let addr = Address::repeat_byte(1);

        index_block(&db, 0, &[make_log(addr, &[])]);
        index_block(&db, 1, &[make_log(addr, &[])]);
        index_block(&db, 2, &[make_log(addr, &[])]);

        // Roll back blocks 0, 1, 2 in one batch.
        let mut batch = db.new_write_batch();
        let mut cache = BitmapCache::default();
        for block in 0u64..=2 {
            deindex_logs(&db, &mut cache, block, &[make_log(addr, &[])]).unwrap();
        }
        cache.flush(&mut batch);
        db.write(batch).unwrap();

        let (bitmap, _) = query(
            &db,
            RepositoryCF::LogBlocksByAddress,
            addr.as_slice(),
            0..10,
        )
        .unwrap();
        assert!(
            bitmap.is_empty(),
            "expected empty bitmap after multi-block rollback, got {bitmap:?}"
        );
    }
}
