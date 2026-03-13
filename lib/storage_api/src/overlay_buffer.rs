use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use alloy::primitives::{B256, BlockNumber};
use anyhow::bail;
use zksync_os_interface::types::StorageWrite;

use crate::state_override_view::OverrideProvider;
use crate::{OverriddenStateView, ReadStateHistory, ViewState};

#[derive(Debug, Clone)]
pub struct BlockOverlay {
    pub storage_writes: HashMap<B256, B256>,
    pub preimages: HashMap<B256, Vec<u8>>,
}

#[derive(Debug, Default, Clone)]
pub struct OverlayBuffer {
    overlays: Arc<BTreeMap<BlockNumber, BlockOverlay>>,
}

impl OverlayBuffer {
    /// Drops records for blocks that already persist in `base`;
    /// Builds a state view to accommodate for the execution of `block_number_to_execute` block.
    /// Note: to execute block N, we need state (+ overlays) to reach N - 1.
    ///
    /// # Invariants
    /// - `block_number_to_execute` is always >= 1. Block 0 is genesis and never goes through
    ///   the execution pipeline. The first block executed is always 1.
    pub fn sync_with_base_and_build_view_for_block<'a, S>(
        &'a mut self,
        base: &'a S,
        block_number_to_execute: BlockNumber,
    ) -> anyhow::Result<
        OverriddenStateView<impl ViewState + 'a, Arc<BTreeMap<BlockNumber, BlockOverlay>>>,
    >
    where
        S: ReadStateHistory + 'a,
    {
        let base_latest = *base.block_range_available().end();
        self.purge_already_persisted_blocks(base_latest)?;
        let first_overlay = self.overlays.keys().next().copied();
        let last_overlay = self.overlays.keys().next_back().copied();
        tracing::debug!(
            "Synced overlay buffer with base (base_latest={base_latest}, overlays_len={}, overlays_range={first_overlay:?}..={last_overlay:?}). \
            Preparing storage view to execute block {block_number_to_execute}.",
            self.overlays.len(),
        );
        if base_latest >= block_number_to_execute - 1 {
            let base_view = base
                .state_view_at(block_number_to_execute - 1)
                .map_err(|e| anyhow::anyhow!(e))?;
            // Return with empty overlays (cheap - Arc to empty BTreeMap)
            return Ok(OverriddenStateView::new(
                base_view,
                Arc::clone(&self.overlays),
            ));
        }

        let base_view = base
            .state_view_at(base_latest)
            .map_err(|e| anyhow::anyhow!(e))?;
        if first_overlay != Some(base_latest + 1)
            || last_overlay != Some(block_number_to_execute - 1)
        {
            // This assert is defensive - we could build overlay maps from a subset of overlay records,
            // but this behaviour is unexpected as we execute blocks in strict accession.
            bail!(
                "Unexpected state of `overlay_buffer` when preparing state view for block {} from base_latest {}; overlays_range={:?}..={:?}",
                block_number_to_execute,
                base_latest,
                first_overlay,
                last_overlay
            );
        }
        Ok(OverriddenStateView::new(
            base_view,
            Arc::clone(&self.overlays),
        ))
    }

    pub fn add_block(
        &mut self,
        block_number: BlockNumber,
        storage_writes: Vec<StorageWrite>,
        preimages: Vec<(B256, Vec<u8>)>,
    ) -> anyhow::Result<()> {
        if let Some(&last) = self.overlays.keys().next_back()
            && block_number != last + 1
        {
            bail!(
                "Overlay head must be contiguous: got {}, expected {}",
                block_number,
                last + 1
            );
        }
        // Convert Vec<StorageWrite> to HashMap for O(1) lookups
        let storage_map: HashMap<B256, B256> = storage_writes
            .into_iter()
            .map(|write| (write.key, write.value))
            .collect();

        let preimage_map: HashMap<B256, Vec<u8>> = preimages.into_iter().collect();

        // INVARIANT: The Arc refcount must be 1 here (no outstanding views holding the Arc).
        // This is guaranteed by the code structure: views are consumed by execute_block_in_vm
        // and dropped before add_block is called. If this assertion fails, we have a logic bug.
        // We are not using references instead because the ` ReadStorage ` trait in VM requires `'static`
        assert_eq!(
            Arc::strong_count(&self.overlays),
            1,
            "Arc refcount > 1 during mutation - this would cause expensive clone!"
        );
        Arc::make_mut(&mut self.overlays).insert(
            block_number,
            BlockOverlay {
                storage_writes: storage_map,
                preimages: preimage_map,
            },
        );
        Ok(())
    }

    fn purge_already_persisted_blocks(&mut self, base_latest: BlockNumber) -> anyhow::Result<()> {
        if let Some(&last) = self.overlays.keys().next()
            && base_latest + 1 < last
        {
            bail!(
                "Cannot clean tail: base_latest {} is behind overlay tail {}",
                base_latest,
                last
            );
        }
        // INVARIANT: Called from sync_with_base_and_build_view_for_block before Arc::clone,
        // so refcount must be 1. If this assertion fails, we have a logic bug.
        assert_eq!(
            Arc::strong_count(&self.overlays),
            1,
            "Arc refcount > 1 during mutation - this would cause expensive clone!"
        );
        Arc::make_mut(&mut self.overlays).retain(|block, _| *block > base_latest);
        Ok(())
    }
}

/// Implement OverrideProvider directly on Arc<BTreeMap<BlockNumber, BlockOverlay>>.
/// Searches through overlays in reverse order (most recent first) with O(1) HashMap lookups per block
impl OverrideProvider for Arc<BTreeMap<BlockNumber, BlockOverlay>> {
    fn get_storage_override(&self, key: &B256) -> Option<B256> {
        for (_, overlay) in self.iter().rev() {
            if let Some(&value) = overlay.storage_writes.get(key) {
                return Some(value);
            }
        }
        None
    }

    fn get_preimage_override(&self, hash: &B256) -> Option<Vec<u8>> {
        for (_, overlay) in self.iter().rev() {
            if let Some(bytes) = overlay.preimages.get(hash) {
                return Some(bytes.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use zksync_os_interface::types::StorageWrite;

    #[test]
    fn overlay_provider_returns_most_recent_value() {
        let key1 = B256::from([1u8; 32]);
        let key2 = B256::from([2u8; 32]);

        let old_value = B256::from([10u8; 32]);
        let new_value = B256::from([20u8; 32]);
        let another_value = B256::from([30u8; 32]);

        let mut buffer = OverlayBuffer::default();

        // Block 1: Write old_value to key1
        buffer
            .add_block(
                1,
                vec![StorageWrite {
                    key: key1,
                    value: old_value,
                    account: Address::ZERO,
                    account_key: B256::ZERO,
                }],
                vec![],
            )
            .unwrap();

        // Block 2: Overwrite key1 with new_value, and write to key2
        buffer
            .add_block(
                2,
                vec![
                    StorageWrite {
                        key: key1,
                        value: new_value,
                        account: Address::ZERO,
                        account_key: B256::ZERO,
                    },
                    StorageWrite {
                        key: key2,
                        value: another_value,
                        account: Address::ZERO,
                        account_key: B256::ZERO,
                    },
                ],
                vec![],
            )
            .unwrap();

        let provider = Arc::clone(&buffer.overlays);

        // Most recent value for key1 should be new_value (from block 2), not old_value (from block 1)
        assert_eq!(
            provider.get_storage_override(&key1),
            Some(new_value),
            "Overlay should return most recent value for key1"
        );

        // key2 should return another_value
        assert_eq!(
            provider.get_storage_override(&key2),
            Some(another_value),
            "Overlay should return value for key2"
        );

        // Non-existent key should return None
        let non_existent_key = B256::from([99u8; 32]);
        assert_eq!(
            provider.get_storage_override(&non_existent_key),
            None,
            "Non-existent key should return None"
        );
    }

    #[test]
    fn overlay_provider_searches_blocks_in_reverse_order() {
        let key = B256::from([1u8; 32]);

        let mut buffer = OverlayBuffer::default();

        // Add 5 blocks, each writing the block number as the value
        for block_num in 1..=5 {
            buffer
                .add_block(
                    block_num,
                    vec![StorageWrite {
                        key,
                        value: B256::from([block_num as u8; 32]),
                        account: Address::ZERO,
                        account_key: B256::ZERO,
                    }],
                    vec![],
                )
                .unwrap();
        }

        let provider = Arc::clone(&buffer.overlays);

        // Should return value from block 5 (most recent)
        assert_eq!(
            provider.get_storage_override(&key),
            Some(B256::from([5u8; 32])),
            "Should return value from most recent block"
        );
    }

    #[test]
    fn preimage_override_returns_most_recent() {
        let hash1 = B256::from([1u8; 32]);
        let hash2 = B256::from([2u8; 32]);

        let old_preimage = vec![10u8; 10];
        let new_preimage = vec![20u8; 20];

        let mut buffer = OverlayBuffer::default();

        // Block 1: Add old preimage
        buffer
            .add_block(1, vec![], vec![(hash1, old_preimage.clone())])
            .unwrap();

        // Block 2: Override hash1 with new preimage, add hash2
        buffer
            .add_block(
                2,
                vec![],
                vec![(hash1, new_preimage.clone()), (hash2, vec![30u8; 30])],
            )
            .unwrap();

        let provider = Arc::clone(&buffer.overlays);

        // Should return new_preimage for hash1, not old_preimage
        assert_eq!(
            provider.get_preimage_override(&hash1),
            Some(new_preimage),
            "Should return most recent preimage for hash1"
        );
    }

    #[test]
    fn purge_removes_persisted_blocks() {
        let mut buffer = OverlayBuffer::default();

        // Add blocks 1-5
        for block_num in 1..=5 {
            buffer.add_block(block_num, vec![], vec![]).unwrap();
        }

        assert_eq!(buffer.overlays.len(), 5);

        // Purge blocks <= 3
        buffer.purge_already_persisted_blocks(3).unwrap();

        // Should only have blocks 4 and 5 left
        assert_eq!(buffer.overlays.len(), 2);
        assert!(buffer.overlays.contains_key(&4));
        assert!(buffer.overlays.contains_key(&5));
        assert!(!buffer.overlays.contains_key(&3));
    }
}
