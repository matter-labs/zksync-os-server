use crate::util;
use alloy::primitives::BlockNumber;
use anyhow::Context;
use std::collections::HashMap;
use std::ops;
use std::sync::{Arc, RwLock};
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;

#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
}

impl CommittedBatchProvider {
    pub async fn init(
        l1_state: &L1State,
        max_l1_blocks_to_scan: u64,
        load_genesis_batch_info: impl AsyncFnOnce() -> StoredBatchInfo,
    ) -> anyhow::Result<Self> {
        let mut inner = Inner::default();
        // Special case for genesis
        if l1_state.last_executed_batch == 0 {
            let batch_info = load_genesis_batch_info().await;
            let batch_hash_l1 = l1_state.diamond_proxy.stored_batch_hash(0).await?;
            anyhow::ensure!(
                batch_hash_l1 == batch_info.hash(),
                "genesis batch hash mismatch: L1 {}, local {}",
                batch_hash_l1,
                batch_info.hash(),
            );
            inner.batches.insert(
                0,
                DiscoveredCommittedBatch {
                    batch_info,
                    block_range: 0..=0,
                },
            );
        }
        // todo: this can take a while and should ideally happen in the background
        // Ignore genesis here as it was handled above
        for batch_number in l1_state.last_executed_batch.max(1)..=l1_state.last_committed_batch {
            let l1_block_with_commit = util::find_l1_commit_block_by_batch_number(
                l1_state.diamond_proxy.clone(),
                batch_number,
                max_l1_blocks_to_scan,
            )
            .await?;
            let discovered_batch = util::fetch_stored_batch_data(
                &l1_state.diamond_proxy,
                l1_block_with_commit,
                batch_number,
            )
            .await?
            .with_context(|| format!("failed to find committed batch {batch_number} on L1"))?;
            tracing::info!(
                batch_number = discovered_batch.number(),
                "discovered committed batch on startup"
            );
            inner.batches.insert(batch_number, discovered_batch);
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub(crate) fn insert(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.batches.insert(batch.batch_info.batch_number, batch);
    }

    pub fn get(&self, batch_number: u64) -> Option<DiscoveredCommittedBatch> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.batches.get(&batch_number).cloned()
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredCommittedBatch {
    /// Information about committed batch as was discovered on-chain.
    pub batch_info: StoredBatchInfo,
    /// Range of L2 blocks that belong to this batch.
    pub block_range: ops::RangeInclusive<BlockNumber>,
}

impl DiscoveredCommittedBatch {
    pub fn number(&self) -> u64 {
        self.batch_info.batch_number
    }

    pub fn first_block(&self) -> BlockNumber {
        *self.block_range.start()
    }

    pub fn last_block(&self) -> BlockNumber {
        *self.block_range.end()
    }

    pub fn block_count(&self) -> u64 {
        self.block_range.end() - self.block_range.start() + 1
    }
}
