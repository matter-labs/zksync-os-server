mod preimages;
mod storage;

use alloy::primitives::{B256, BlockNumber};
use std::path::PathBuf;
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_interface::types::StorageWrite;
use zksync_os_storage_api::{ReadStateHistory, StateError, StateResult, ViewState, WriteState};

use preimages::FullDiffsPreimages;
use storage::FullDiffsStorage;
use zksync_os_genesis::Genesis;

const STATE_STORAGE_DB_NAME: &str = "state_full_diffs";
const PREIMAGES_STORAGE_DB_NAME: &str = "preimages_full_diffs";

#[derive(Debug, Clone)]
pub struct FullDiffsState {
    storage: FullDiffsStorage,
    preimages: FullDiffsPreimages,
}

impl FullDiffsState {
    /// Creates a new state using RocksDB at the provided directory. The directory will contain two DBs.
    pub async fn new(base_path: PathBuf, genesis: &Genesis) -> anyhow::Result<Self> {
        let storage = FullDiffsStorage::new(&base_path.join(STATE_STORAGE_DB_NAME))?;
        let preimages = FullDiffsPreimages::new(&base_path.join(PREIMAGES_STORAGE_DB_NAME))?;

        let this = Self { storage, preimages };
        if this.storage.latest_block() == 0 {
            let storage_logs = genesis
                .state()
                .await
                .storage_logs
                .clone()
                .into_iter()
                .map(|(key, value)| StorageWrite {
                    key,
                    value,
                    // todo: `account` and `account_key` are not used in this crate -
                    // we should just have (key, value) pairs in the interface
                    account: Default::default(),
                    account_key: Default::default(),
                })
                .collect();

            let force_deploy_preimages = genesis.genesis_upgrade_tx().await.force_deploy_preimages;
            let preimages = genesis
                .state()
                .await
                .preimages
                .iter()
                .chain(force_deploy_preimages.iter())
                .map(|(k, v)| (*k, v));

            this.add_block_result(0, storage_logs, preimages, false)?
        }

        Ok(this)
    }
}

#[derive(Debug, Clone)]
pub struct StateViewFD {
    storage: FullDiffsStorage,
    preimages: FullDiffsPreimages,
    block: u64,
}

impl ReadStorage for StateViewFD {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.storage.read_at(self.block, key)
    }
}

impl PreimageSource for StateViewFD {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.preimages.get(hash)
    }
}

impl ReadStateHistory for FullDiffsState {
    fn state_view_at(&self, block_number: BlockNumber) -> StateResult<impl ViewState> {
        let latest = self.storage.latest_block();
        if block_number > latest {
            return Err(StateError::NotFound(block_number));
        }
        Ok(StateViewFD {
            storage: self.storage.clone(),
            preimages: self.preimages.clone(),
            block: block_number,
        })
    }

    fn block_range_available(&self) -> std::ops::RangeInclusive<u64> {
        0..=self.storage.latest_block()
    }
}

impl WriteState for FullDiffsState {
    fn add_block_result<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
        override_allowed: bool,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (B256, &'a Vec<u8>)>,
    {
        self.storage
            .add_block(block_number, storage_diffs, override_allowed)?;
        self.preimages.add(new_preimages)?;
        Ok(())
    }
}
