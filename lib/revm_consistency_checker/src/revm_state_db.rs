use std::{error::Error, fmt};

use crate::helpers::get_unpadded_code;
use alloy::primitives::{Address, B256, KECCAK256_EMPTY};
use reth_revm::{
    DatabaseRef,
    db::DBErrorMarker,
    primitives::{StorageKey, StorageValue},
    state::{AccountInfo, Bytecode},
};
use ruint::aliases::B160;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_forward_system::run::ReadStorage;
use zksync_os_interface::{traits::PreimageSource, types::BlockHashes};
use zksync_os_merkle_tree::fixed_bytes_to_bytes32;
use zksync_os_storage_api::ReadStateHistory;
use zksync_os_storage_api::{StateError, ViewState};

#[derive(Debug, Clone)]
pub struct RevmStateDb<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    state: State,
    latest_block: u64,
    block_hashes: BlockHashes,
}

impl<State> RevmStateDb<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    pub fn new(state: State, latest_block: u64, block_hashes: BlockHashes) -> Self {
        RevmStateDb {
            state,
            latest_block,
            block_hashes,
        }
    }

    pub fn set_latest_block(&mut self, latest_block: u64, block_hashes: BlockHashes) {
        self.latest_block = latest_block;
        self.block_hashes = block_hashes;
    }
}

#[derive(Debug)]
pub struct RevmStateDbError(anyhow::Error);

// add this:
impl DBErrorMarker for RevmStateDbError {}

impl fmt::Display for RevmStateDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for RevmStateDbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

// convenient conversions
impl From<anyhow::Error> for RevmStateDbError {
    fn from(e: anyhow::Error) -> Self {
        RevmStateDbError(e)
    }
}

impl From<StateError> for RevmStateDbError {
    fn from(e: StateError) -> Self {
        RevmStateDbError(e.into())
    }
}

impl<State> DatabaseRef for RevmStateDb<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    /// The database error type.
    type Error = RevmStateDbError;

    /// Gets basic account information.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.state
            .state_view_at(self.latest_block)?
            .get_account(address)
            .map(|props| -> Result<_, Self::Error> {
                let observable_code_hash = {
                    let is_acc_empty = props.nonce == 0 && props.balance.is_zero();
                    if props.observable_bytecode_hash.is_zero() && !is_acc_empty {
                        KECCAK256_EMPTY
                    } else {
                        B256::from(props.observable_bytecode_hash.as_u8_array())
                    }
                };

                let code = if props.bytecode_hash.is_zero() {
                    None
                } else {
                    let bytecode =
                        self.code_by_hash_ref(B256::from(props.bytecode_hash.as_u8_array()))?;
                    Some(get_unpadded_code(bytecode.bytes_slice(), &props))
                };

                Ok(AccountInfo {
                    nonce: props.nonce,
                    balance: props.balance,
                    code_hash: observable_code_hash,
                    code,
                })
            })
            .transpose()
    }

    /// Gets account code by its hash.
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .state
            .state_view_at(self.latest_block)?
            .get_preimage(code_hash)
            .map(|bytes| Bytecode::new_raw(bytes.into()))
            .unwrap_or_default())
    }

    /// Gets storage value of address at index.
    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let flat_key = derive_flat_storage_key(
            &B160::from_be_bytes(address.into_array()),
            &fixed_bytes_to_bytes32(index.into()),
        );
        Ok(self
            .state
            .state_view_at(self.latest_block)?
            .read(flat_key)
            .unwrap_or_default()
            .into_u256_be())
    }

    /// Gets block hash by block number.
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(diff) = self.latest_block.checked_sub(number)
            && diff < 256
        {
            Ok(self.block_hashes.0[255 - diff as usize].into())
        } else {
            Ok(B256::default())
        }
    }
}
