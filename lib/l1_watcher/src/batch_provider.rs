use crate::{StoredBatchData, util};
use anyhow::Context;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use zksync_os_contract_interface::l1_discovery::L1State;

pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
}

impl CommittedBatchProvider {
    pub async fn init(l1_state: &L1State, max_l1_blocks_to_scan: u64) -> anyhow::Result<Self> {
        let mut inner = Inner {
            batches: Default::default(),
        };
        for batch_number in l1_state.last_executed_batch + 1..=l1_state.last_committed_batch {
            let stored_batch_data = util::find_stored_batch_data_by_batch_number(
                &l1_state.diamond_proxy,
                batch_number,
                max_l1_blocks_to_scan,
            )
            .await?
            .with_context(|| format!("failed to find committed batch {} on L1", batch_number))?;
            inner.batches.insert(batch_number, stored_batch_data);
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub(crate) fn add(&self, batch: StoredBatchData) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.batches.insert(batch.batch_info.batch_number, batch);
    }

    pub fn get(&self, batch_number: u64) -> Option<StoredBatchData> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.batches.get(&batch_number).cloned()
    }
}

struct Inner {
    batches: HashMap<u64, StoredBatchData>,
}
