use alloy::primitives::BlockNumber;
use alloy::primitives::ruint::aliases::B160;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zk_os_forward_system::run::{PreimageSource, ReadStorageTree};

/// Read-only view on a state from a specific block.
pub trait ViewState: ReadStorageTree + PreimageSource + Send + Clone {
    fn get_account(&mut self, address: B160) -> Option<AccountProperties> {
        let key = derive_flat_storage_key(
            &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
            &address_into_special_storage_key(&address),
        );
        self.read(key).map(|hash| {
            AccountProperties::decode(&self.get_preimage(hash).unwrap().try_into().unwrap())
        })
    }
}

impl<T: ReadStorageTree + PreimageSource + Send + Clone> ViewState for T {}

/// Read-only history of state views.
pub trait ReadStateHistory: Send + Sync + 'static {
    /// Get a view on state from the given block.
    fn state_view_at(&self, block_number: BlockNumber) -> StateResult<impl ViewState>;
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
