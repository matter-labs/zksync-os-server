use crate::util;
use alloy::primitives::BlockNumber;
use anyhow::Context;
use rangemap::RangeInclusiveMap;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;

#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
    block_range_index: RangeInclusiveMap<BlockNumber, u64>,
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
            let batch_hash_l1 = l1_state.diamond_proxy_l1.stored_batch_hash(0).await?;
            anyhow::ensure!(
                batch_hash_l1 == batch_info.hash(),
                "genesis batch hash mismatch: L1 {}, local {}",
                batch_hash_l1,
                batch_info.hash(),
            );
            inner.insert(DiscoveredCommittedBatch {
                batch_info,
                block_range: 0..=0,
            });
        }
        // todo: this can take a while and should ideally happen in the background
        // Ignore genesis here as it was handled above
        for batch_number in l1_state.last_executed_batch.max(1)..=l1_state.last_committed_batch {
            let discovered_batch =
                discover_committed_batch_for_startup(l1_state, batch_number, max_l1_blocks_to_scan)
                    .await?;
            tracing::info!(
                batch_number = discovered_batch.number(),
                "discovered committed batch on startup"
            );
            inner.insert(discovered_batch);
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub(crate) fn insert(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.insert(batch);
    }

    pub fn get(&self, batch_number: u64) -> Option<DiscoveredCommittedBatch> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.batches.get(&batch_number).cloned()
    }

    pub fn get_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> Option<DiscoveredCommittedBatch> {
        let inner = self.inner.read().expect("lock poisoned");
        let batch_number = inner.block_range_index.get(&block_number)?;
        inner.batches.get(batch_number).cloned()
    }
}

impl Inner {
    fn insert(&mut self, batch: DiscoveredCommittedBatch) {
        self.block_range_index
            .insert(batch.block_range.clone(), batch.number());
        self.batches.insert(batch.number(), batch);
    }
}

/// For chains that migrated from L1 settlement to a gateway, pre-migration commit events are on
/// `diamond_proxy_l1`; post-migration commits are on `diamond_proxy_sl`.
async fn discover_committed_batch_for_startup(
    l1_state: &L1State,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<DiscoveredCommittedBatch> {
    // Same diamond address can appear on L1 and on the gateway L2; use chain IDs, not addresses.
    let settle_on_l1 = l1_state.l1_chain_id == l1_state.sl_chain_id;
    if settle_on_l1 {
        let block = util::find_l1_commit_block_by_batch_number(
            l1_state.diamond_proxy_sl.clone(),
            batch_number,
            max_l1_blocks_to_scan,
        )
        .await?;
        let discovered =
            util::fetch_stored_batch_data(&l1_state.diamond_proxy_sl, block, batch_number)
                .await?
                .with_context(|| format!("failed to find committed batch {batch_number} on L1"))?;
        return Ok(discovered);
    }
    match util::find_l1_commit_block_by_batch_number(
        l1_state.diamond_proxy_l1.clone(),
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await
    {
        Ok(block) => {
            if let Some(batch) =
                util::fetch_stored_batch_data(&l1_state.diamond_proxy_l1, block, batch_number)
                    .await?
            {
                tracing::debug!(batch_number, "found committed batch on L1 diamond proxy");
                return Ok(batch);
            }
        }
        Err(err) => {
            tracing::warn!(
                batch_number,
                ?err,
                "failed to find commit block on L1 diamond proxy, falling back to settlement layer"
            );
        }
    }
    let block = util::find_l1_commit_block_by_batch_number(
        l1_state.diamond_proxy_sl.clone(),
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?;
    let batch = util::fetch_stored_batch_data(&l1_state.diamond_proxy_sl, block, batch_number)
        .await?
        .with_context(|| {
            format!(
                "failed to find committed batch {batch_number} on L1 diamond or settlement-layer chain"
            )
        })?;
    tracing::debug!(batch_number, "found committed batch on settlement layer");
    Ok(batch)
}
