use std::collections::HashMap;
use zksync_os_storage_api::ReadFinality;

use super::metrics::BATCH_VERIFICATION_RESPONDER_METRICS;

/// Cache of blocks that are to be used for batch verification
/// Accepts blocks only in ascending order. Old blocks are evicted when not
/// needed through a call to remove_lower_then.
///
/// This may be optimized by using a ring buffer for data storage instead.
pub(super) struct BlockCache<Finality, Data> {
    data: HashMap<u64, Data>,
    /// Range of cached data. Range is inclusive of both bounds.
    range: Option<(u64, u64)>,
    finality: Finality,
}

impl<Finality: ReadFinality, Data> BlockCache<Finality, Data> {
    pub fn new(finality: Finality) -> Self {
        Self {
            data: HashMap::new(),
            range: None,
            finality,
        }
    }

    /// Insert a block into the cache. Expected blocks to be added in order.
    pub fn insert(&mut self, block_number: u64, block: Data) -> anyhow::Result<()> {
        if let Some((low, high)) = self.range {
            if block_number != high + 1 {
                anyhow::bail!("Out of order block received. This should never happen");
            }
            self.range = Some((low, block_number));
        } else {
            self.range = Some((block_number, block_number));
        }
        self.data.insert(block_number, block);

        // evict block for committed batches
        self.remove_lower_then(self.finality.get_finality_status().last_committed_block + 1);

        if let Some((start, end)) = self.range {
            BATCH_VERIFICATION_RESPONDER_METRICS.update_cache_range(start, end);
        } else {
            // some synthetic value that will be ok on a graph. size is right (empty)
            BATCH_VERIFICATION_RESPONDER_METRICS
                .update_cache_range(block_number, block_number.saturating_sub(1));
        }
        Ok(())
    }

    pub fn get(&self, block_number: u64) -> Option<&Data> {
        self.data.get(&block_number)
    }

    /// Removes all blocks lower than the given block number
    pub fn remove_lower_then(&mut self, block_number: u64) {
        if let Some((low, high)) = self.range {
            for num in low..block_number {
                self.data.remove(&num);
            }
            let new_range = (block_number, high);

            if new_range.0 > new_range.1 {
                self.range = None;
            } else {
                self.range = Some(new_range);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::DummyFinality;

    use super::*;

    // We use everywhere zero finality to trigger no automatic evictions in tests.

    #[test]
    fn insert_updates_range_and_data_for_sequential_blocks() {
        let finality = DummyFinality::zero();
        let mut cache = BlockCache::<_, u64>::new(finality);

        cache.insert(1, 1).unwrap();
        assert_eq!(cache.range, Some((1, 1)));
        cache.insert(2, 2).unwrap();
        cache.insert(3, 3).unwrap();

        assert_eq!(cache.data.len(), 3);
        assert_eq!(cache.get(1), Some(&1));
        assert_eq!(cache.get(2), Some(&2));
        assert_eq!(cache.get(3), Some(&3));
        assert_eq!(cache.range, Some((1, 3)));
    }

    #[test]
    fn insert_rejects_out_of_order_blocks() {
        let finality = DummyFinality::zero();
        let mut cache = BlockCache::new(finality);

        cache.insert(1, 1).unwrap();
        cache.insert(2, 2).unwrap();

        // Inserting a block that is not exactly `high + 1` should fail.
        let err = cache.insert(4, 4).unwrap_err();
        assert!(err.to_string().contains("Out of order block received"));
        // Existing state must be unchanged.
        assert_eq!(cache.range, Some((1, 2)));
        assert_eq!(cache.get(1), Some(&1));
        assert_eq!(cache.get(2), Some(&2));
        assert!(cache.get(4).is_none());

        let err = cache.insert(1, 1).unwrap_err();
        assert!(err.to_string().contains("Out of order block received"));

        // Existing state must be unchanged.
        assert_eq!(cache.range, Some((1, 2)));
        assert_eq!(cache.get(1), Some(&1));
        assert_eq!(cache.get(2), Some(&2));
        assert!(cache.get(4).is_none());
    }

    #[test]
    fn remove_lower_then_evicts_and_updates_range() {
        let finality = DummyFinality::zero();
        let mut cache = BlockCache::new(finality);

        for n in 1u64..=5 {
            cache.insert(n, n).unwrap();
        }
        assert_eq!(cache.range, Some((1, 5)));

        // Evict blocks below 3
        cache.remove_lower_then(3);
        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_none());
        assert_eq!(cache.get(3), Some(&3));
        assert_eq!(cache.get(4), Some(&4));
        assert_eq!(cache.get(5), Some(&5));
        assert_eq!(cache.range, Some((3, 5)));

        // Evict everything
        cache.remove_lower_then(6);
        assert!(cache.data.is_empty());
        assert_eq!(cache.range, None);
    }
}
