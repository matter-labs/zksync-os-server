use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, BlockNumber, U256};
use std::fmt::Debug;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_interface::types::StorageWrite;

/// Read-only view on a state from a specific block.
pub trait ViewState: ReadStorage + PreimageSource + Send + Clone {
    fn get_account(&mut self, address: Address) -> Option<AccountProperties> {
        let key = derive_flat_storage_key(
            &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
            &address_into_special_storage_key(&B160::from_be_bytes(address.into_array())),
        );
        self.read(B256::from(key.as_u8_array())).map(|hash| {
            AccountProperties::decode(&self.get_preimage(hash).unwrap().try_into().unwrap())
        })
    }

    /// Get account's nonce by its address.
    ///
    /// Returns `None` if the account doesn't exist
    fn nonce(&mut self, address: Address) -> Option<u64> {
        self.get_account(address).map(|a| a.nonce)
    }

    /// Get account's balance by its address. Returns zero for non-existent accounts.
    fn balance(&mut self, address: Address) -> U256 {
        self.get_account(address)
            .map(|a| a.balance)
            .unwrap_or_default()
    }
}

impl<T: ReadStorage + PreimageSource + Send + Clone> ViewState for T {}

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
        override_allowed: bool,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (B256, &'a Vec<u8>)>;
}

/// First three bytes of an EIP-7702 delegation designator: `0xef01` magic + version `0x00`.
/// Matches `EIP7702_DELEGATION_MARKER` in ZKsync OS.
const EIP7702_DELEGATION_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];
/// Full designator length: 3-byte prefix + 20-byte address.
const EIP7702_DELEGATION_DESIGNATOR_LEN: usize = 23;

/// If `preimage` starts with a delegation designator, returns the exact 23-byte designator;
/// ZKsync OS stores it padded with trailing zeroes, but revm's 7702 parser requires exactly
/// 23 bytes. Returns `None` for regular bytecode.
pub fn eip7702_delegation_designator(preimage: &[u8]) -> Option<&[u8]> {
    (preimage.len() >= EIP7702_DELEGATION_DESIGNATOR_LEN
        && preimage.starts_with(&EIP7702_DELEGATION_PREFIX))
    .then(|| &preimage[..EIP7702_DELEGATION_DESIGNATOR_LEN])
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
