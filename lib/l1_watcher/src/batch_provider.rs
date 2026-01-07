use crate::util;
use alloy::primitives::BlockNumber;
use anyhow::Context;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;

#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
}

impl CommittedBatchProvider {
    pub async fn init(l1_state: &L1State, max_l1_blocks_to_scan: u64) -> anyhow::Result<Self> {
        let mut inner = Inner {
            batches: Default::default(),
        };
        for batch_number in l1_state.last_executed_batch + 1..=l1_state.last_committed_batch {
            let l1_block_with_commit = util::find_l1_commit_block_by_batch_number(
                l1_state.diamond_proxy.clone(),
                batch_number,
                max_l1_blocks_to_scan,
            )
            .await?;
            let stored_batch_data = util::fetch_stored_batch_data(
                &l1_state.diamond_proxy,
                l1_block_with_commit,
                batch_number,
            )
            .await?
            .with_context(|| format!("failed to find committed batch {} on L1", batch_number))?;
            inner.batches.insert(
                batch_number,
                DiscoveredCommittedBatch {
                    batch_info: stored_batch_data.batch_info,
                    first_block_number: stored_batch_data.first_block_number,
                    last_block_number: stored_batch_data.last_block_number,
                    commit_l1_block_number: l1_block_with_commit,
                },
            );
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub(crate) fn add(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.batches.insert(batch.batch_info.batch_number, batch);
    }

    pub fn get(&self, batch_number: u64) -> Option<DiscoveredCommittedBatch> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.batches.get(&batch_number).cloned()
    }
}

#[derive(Debug)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredCommittedBatch {
    /// Information about committed batch as was discovered on-chain.
    pub batch_info: StoredBatchInfo,
    /// First L2 block that belongs to this batch.
    pub first_block_number: BlockNumber,
    /// Last L2 block that belongs to this batch.
    pub last_block_number: BlockNumber,
    /// L1 block number where this batch was committed.
    pub commit_l1_block_number: BlockNumber,
}

impl DiscoveredCommittedBatch {
    pub fn number(&self) -> u64 {
        self.batch_info.batch_number
    }
}
