use crate::conversions::{bytes32_to_address, h256_to_bytes32};
use crate::storage::in_memory_account_properties::InMemoryAccountProperties;
use crate::storage::in_memory_block_receipts::InMemoryBlockReceipts;
use crate::storage::in_memory_tx_receipts::InMemoryTxReceipts;
use crate::storage::persistent_storage_map::PersistentStorageMap;
use crate::storage::rocksdb_preimages::RocksDbPreimages;
use crate::storage::storage_map::{StorageMap, StorageMapView};
use crate::storage::storage_metrics::StorageMetrics;
use crate::tx_conversions::transaction_to_api_data;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zk_ee::common_structs::PreimageType;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    AccountProperties, ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
};
use zk_os_forward_system::run::{BatchOutput, PreimageSource, ReadStorage, ReadStorageTree};
use zksync_types::{Address, Transaction};

pub mod block_replay_storage;
pub mod in_memory_account_properties;
pub mod in_memory_block_receipts;
pub mod in_memory_preimages;
pub mod in_memory_tx_receipts;
pub mod persistent_storage_map;
pub mod rocksdb_preimages;
pub mod storage_map;
mod storage_metrics;

/// This is a handle to the in-memory state of the sequencer.
/// It's composed of multiple facets - note that they don't interact with each other and are generally lock-free.
/// all are thread-safe
/// we have two threshold block numbers:
/// - last_pending_block_number -   highest block number that has its results available in the state facets -
///   potentially not canonized yet. Must not be exposed for API directly, but can be used for nonce/balance validation.
/// - last_canonized_block_number - the highest canonized block number - can be exposed in API.

#[derive(Clone, Debug)]
pub struct StateHandle(pub Arc<StateHandleInner>);

// todo: should probably also store the oldest block number that is guaranteed to be available in all the state facets
// todo: we have Arcs above and inside each. check if needed.
#[derive(Debug)]
pub struct StateHandleInner {
    pub last_pending_block_number: Arc<AtomicU64>,

    pub last_canonized_block_number: Arc<AtomicU64>,

    // facets - updated and accessed independently

    // stores full state -
    // per-block diff for ~last BLOCKS_TO_RETAIN blocks and compacted base state
    pub in_memory_storage: StorageMap,

    // preimages are currently stored sync in RocksDB
    pub rocks_db_preimages: RocksDbPreimages,

    // stores account properties of all accounts -
    // per-block diff for ~last BLOCKS_TO_RETAIN blocks and compacted values for blocks before
    pub account_property_history: InMemoryAccountProperties,

    // simple thread-safe HashMap<Block, BatchOutput>
    pub in_memory_block_receipts: InMemoryBlockReceipts,
    // simple thread-safe HashMap<TxHash, TxReceipt>
    pub in_memory_tx_receipts: InMemoryTxReceipts,
}

// Provides execution environment for block number `block`
// Delegates storage reads to StorageMapView and preimages to RocksDbPreimages
// i.e., all storage values are as of end of block `block - 1`
// preimages may be returned from future blocks (confirmed as acceptable - but may still reconsider)
#[derive(Clone, Debug)]
pub struct StorageView {
    _block: u64,
    storage_map_view: StorageMapView,

    preimages: RocksDbPreimages,
}

impl StateHandle {
    /// Returns a `StorageView` for reading state at `block_number`.
    /// Contains changes from up to `block_number - 1`.
    /// todo: for now the caller must ensure `block_number >= base_block`
    pub fn view_at(&self, block_number: u64) -> anyhow::Result<StorageView> {
        let last_block = self.0.last_pending_block_number.load(Ordering::Relaxed);
        // tracing::info!("Creating StorageView for block {} (last pending: {})", block_number, last_block);
        if block_number > last_block + 1 {
            return Err(anyhow::anyhow!(
                "Cannot create StorageView for future block {} (current is {})",
                block_number,
                last_block
            ));
        }
        let storage_map_view = self.0.in_memory_storage.view_at(block_number)?;
        let r = StorageView {
            _block: block_number,
            storage_map_view,
            preimages: self.0.rocks_db_preimages.clone(),
        };
        Ok(r)
    }

    // todo: think about the best way to pass dependencies -
    //   currently `PersistentStorageMap` is constructed outside to get block number to replay txs from
    pub fn new(
        last_persisted_block: u64,
        persistent_storage_map: PersistentStorageMap,
        rocks_db_preimages: RocksDbPreimages,
        blocks_to_retain: usize,
    ) -> StateHandle {
        let last_pending_block_number = Arc::new(AtomicU64::new(last_persisted_block));
        let last_canonized_block_number = Arc::new(AtomicU64::new(last_persisted_block));

        StateHandle(Arc::new(StateHandleInner {
            last_pending_block_number,
            last_canonized_block_number,
            in_memory_storage: StorageMap::new(persistent_storage_map, blocks_to_retain),
            rocks_db_preimages,
            account_property_history: InMemoryAccountProperties::empty(blocks_to_retain),
            in_memory_block_receipts: InMemoryBlockReceipts::empty(blocks_to_retain),
            in_memory_tx_receipts: InMemoryTxReceipts::empty(),
        }))
    }

    // Advances the last canonized block number;
    // asserts that the new block number is next in sequence.
    // Should only be called when blocks before and including `new_pending_block_number` are durable.
    pub fn advance_canonized_block(&self, new_canonized_block_number: u64) {
        let prev_last_canonized_block_number =
            self.0.last_canonized_block_number.load(Ordering::Relaxed);
        tracing::info!(
            "Advancing canonized block from {} to {}",
            prev_last_canonized_block_number,
            new_canonized_block_number
        );
        assert_eq!(
            prev_last_canonized_block_number + 1,
            new_canonized_block_number,
            "Block number must be strictly increasing: expected {}, got {}",
            prev_last_canonized_block_number + 1,
            new_canonized_block_number
        );

        self.0
            .last_canonized_block_number
            .store(new_canonized_block_number, Ordering::Relaxed);
    }

    pub fn handle_block_output(
        &self,
        block_output: BatchOutput,
        //todo: process separately
        transactions: Vec<Transaction>,
    ) {
        tracing::info!(
        "Handling block output for block {} ({} txs) with {} preimages and {} storage log writes",
            block_output.header.number,
            block_output.tx_results.len(),
            block_output.published_preimages.len(),
            block_output.storage_writes.len(),
        );
        // block_output.tx_results.iter().for_each(|tx_result| {
        //     match tx_result {
        //         Ok(res) => {
        //             tracing::info!(
        //                 "Transaction logs: {:?}",
        //                 res.logs,
        //             );
        //         }
        //         Err(_) => {
        //             tracing::warn!(
        //                 "Transaction failed: {:?}",
        //                 tx_result,
        //             );
        //         }
        //     }
        // });
        // block_output.storage_writes.iter().for_each(|log| {
        // tracing::info!(
        // "Storage write: account: {:?}, account: {:?}, key: {:?}, value: {:?}",
        // log.account,
        // log.account_key,
        // log.account_key,
        // log.value
        // );
        // });

        // let mut _ts = std::time::Instant::now();

        let prev_last_block_number = self.0.last_pending_block_number.load(Ordering::Relaxed);
        let current_block_number = block_output.header.number;
        assert_eq!(
            prev_last_block_number + 1,
            current_block_number,
            "Block number must be strictly increasing: expected {}, got {}",
            prev_last_block_number + 1,
            current_block_number
        );

        // Account properties that were inserted/updated during this block
        // We'll use them to determine balances and nonces for API validation
        // And they'll also need to be stored as preimages for future decommit as well
        let account_properties = self.extract_account_properties(&block_output);

        // tracing::info!("Block {} - saving - prepared acc properties in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();

        // Update the in-memory storage with the new state
        self.0
            .in_memory_storage
            .add_diff(current_block_number, block_output.storage_writes.clone());

        // tracing::info!("Block {} - saving - added to in_memory_storage in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();
        // Update the preimages
        self.0.rocks_db_preimages.add(
            current_block_number,
            block_output
                .published_preimages
                .iter()
                .map(|(hash, preimage, _)| (*hash, preimage.clone())),
        );

        // tracing::info!("Block {} - saving - added to published_preimages in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();

        self.0
            .account_property_history
            .add_diff(current_block_number, account_properties);

        // tracing::info!("Block {} - saving - added to account_property_history in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();

        // Update transaction receipts
        // Note: race condition - we may expose transaction receipt before `last_canonized_block_number` is bumped
        for (index, tx) in transactions.iter().enumerate() {
            let api_tx = transaction_to_api_data(&block_output, index, tx);
            self.0
                .in_memory_tx_receipts
                .insert(h256_to_bytes32(tx.hash()), api_tx);
        }

        // tracing::info!("Block {} - saving - added to in_memory_tx_receipts in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();

        // Update block receipts
        self.0
            .in_memory_block_receipts
            .insert(current_block_number, block_output);

        // tracing::info!("Block {} - saving - added to in_memory_block_receipts in {:?},", current_block_number, ts.elapsed());
        // ts = std::time::Instant::now();

        tracing::info!(
            "Advancing last pending block number from {} to {}",
            prev_last_block_number,
            current_block_number
        );
        // Update the last pending block number
        self.0
            .last_pending_block_number
            .store(current_block_number, Ordering::Relaxed);
    }

    pub fn last_canonized_block_number(&self) -> u64 {
        self.0.last_canonized_block_number.load(Ordering::Relaxed)
    }

    fn extract_account_properties(
        &self,
        block_output: &BatchOutput,
    ) -> HashMap<Address, AccountProperties> {
        let account_properties_preimages: HashMap<Bytes32, AccountProperties> = block_output
            .published_preimages
            .iter()
            .filter_map(|(hash, preimage, preimage_type)| match preimage_type {
                PreimageType::Bytecode => None,
                PreimageType::AccountData => Some((
                    *hash,
                    AccountProperties::decode(
                        &preimage
                            .clone()
                            .try_into()
                            .expect("Preimage should be exactly 124 bytes"),
                    ),
                )),
            })
            .collect();

        let mut result = HashMap::new();
        for log in &block_output.storage_writes {
            if log.account == ACCOUNT_PROPERTIES_STORAGE_ADDRESS {
                let account_address = bytes32_to_address(&log.account_key);

                if let Some(properties) = account_properties_preimages.get(&log.value) {
                    result.insert(account_address, *properties);
                } else {
                    let ex = self.0.rocks_db_preimages.get(log.value);
                    tracing::warn!(
                        "Account properties preimage not found for address {} and value {:?} - global storage: {}",
                        account_address,
                        log.value,
                        ex.is_some()
                    );
                }
            }
        }

        // if !account_properties_preimages.is_empty() {
        //     panic!("could not map account properties to addresses");
        // }
        result
    }

    pub async fn collect_state_metrics(&self, period: Duration) {
        let mut ticker = tokio::time::interval(period);
        let state_handle = self.clone();
        loop {
            ticker.tick().await;
            let m = StorageMetrics::collect_metrics(state_handle.clone());
            tracing::info!("{:?}", m);
        }
    }

    pub async fn compact_periodically(&self, period: Duration) {
        let mut ticker = tokio::time::interval(period);
        let map = self.0.in_memory_storage.clone();
        // can take more than `period` to comact - use proper scheduler
        loop {
            ticker.tick().await;
            map.compact();
        }
    }
}

impl ReadStorage for StorageView {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.storage_map_view.read(key)
    }
}
impl PreimageSource for StorageView {
    fn get_preimage(&mut self, hash: Bytes32) -> Option<Vec<u8>> {
        self.preimages.get(hash)
    }
}

/* ------------------------------------------------------------------ */
/*  Dummy trait impls required by zk-OS                               */
/* ------------------------------------------------------------------ */
impl ReadStorageTree for StorageView {
    fn tree_index(&mut self, _key: Bytes32) -> Option<u64> {
        unimplemented!()
    }
    fn merkle_proof(&mut self, _idx: u64) -> zk_os_forward_system::run::LeafProof {
        unimplemented!()
    }
    fn prev_tree_index(&mut self, _key: Bytes32) -> u64 {
        unimplemented!()
    }
}
