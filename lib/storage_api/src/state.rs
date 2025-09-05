use alloy::primitives::BlockNumber;
use alloy::primitives::ruint::aliases::B160;
use std::fmt::Debug;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zksync_os_interface::bytes32::Bytes32;
use zksync_os_interface::common_types::StorageWrite;
use zksync_os_interface::traits::{PreimageSource, ReadStorageTree};
// use zksync_os_interface::common_types::derive_flat_storage_key;

/// Read-only view on a state from a specific block.
pub trait ViewState: ReadStorageTree + PreimageSource + Send + Clone {
    fn get_account(&mut self, address: B160) -> Option<AccountProperties> {
        let key = derive_flat_storage_key(
            &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
            &address_into_special_storage_key(&address),
        );
        self.read(zksync_os_interface::bytes32::Bytes32::from_array(
            key.as_u8_array(),
        ))
        .map(|hash| {
            AccountProperties::decode(&self.get_preimage(hash).unwrap().try_into().unwrap())
        })
    }
}

impl<T: ReadStorageTree + PreimageSource + Send + Clone> ViewState for T {}

/// Read-only history of state views.
pub trait ReadStateHistory: Debug + Send + Sync + 'static {
    /// Get a view on state from the given block.
    fn state_view_at(&self, block_number: BlockNumber) -> StateResult<impl ViewState>;

    /// Block numbers whose state diffs are available in state.
    /// Note that the block numbers that can be **run** against this state implementation are
    /// `(block_range_available.min + 1)..=(block_range_available.max + 1)`
    fn block_range_available(&self) -> std::ops::RangeInclusive<u64>;
}

pub trait WriteState: Send + Sync + 'static {
    /// Add given block to state.
    fn add_block_result<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (Bytes32, &'a Vec<u8>)>;
}

/// State reader result type.
pub type StateResult<Ok> = Result<Ok, StateError>;

/// Error variants thrown by state readers.
#[derive(Clone, Debug, thiserror::Error)]
pub enum StateError {
    #[error("block {0} is compacted")]
    Compacted(BlockNumber),
    #[error("block {0} not found")]
    NotFound(BlockNumber),
}
