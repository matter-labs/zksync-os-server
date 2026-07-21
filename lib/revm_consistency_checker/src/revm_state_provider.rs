use crate::helpers::get_unpadded_code;
use alloy::primitives::{Address, B256, Bytes, KECCAK256_EMPTY};
use revm::DatabaseRef;
use revm::bytecode::BytecodeKind;
use revm::database_interface::DBErrorMarker;
use revm::primitives::{StorageKey, StorageValue};
use revm::state::{AccountInfo, Bytecode};
use ruint::aliases::B160;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_ee::utils::Bytes32;
use zksync_os_storage_api::{BlockHashes, ViewState, eip7702_delegation_designator};

fn fixed_bytes_to_bytes32(x: B256) -> Bytes32 {
    let x: [u8; 32] = x.into();
    x.into()
}

/// Build a revm [`Bytecode`] from a ZKsync OS preimage. A 7702 delegation designator is trimmed
/// to its 23 bytes (so revm follows the delegation); regular bytecode is wrapped verbatim.
fn bytecode_from_preimage(full_bytecode: &[u8]) -> Bytecode {
    match eip7702_delegation_designator(full_bytecode) {
        Some(designator) => Bytecode::new_raw_checked(Bytes::copy_from_slice(designator))
            .expect("valid EIP-7702 delegation designator"),
        None => Bytecode::new_raw(Bytes::copy_from_slice(full_bytecode)),
    }
}

#[derive(Debug, Clone)]
pub struct RevmStateProvider<State>
where
    State: ViewState,
{
    state_view: State,
    block_hashes: BlockHashes,
    state_block_number: u64,
}

impl<State> RevmStateProvider<State>
where
    State: ViewState,
{
    pub fn new(state_view: State, block_hashes: BlockHashes, state_block_number: u64) -> Self {
        Self {
            state_view,
            block_hashes,
            state_block_number,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct RevmStateProviderError(#[from] anyhow::Error);

impl DBErrorMarker for RevmStateProviderError {}

impl<State> DatabaseRef for RevmStateProvider<State>
where
    State: ViewState,
{
    /// The database error type.
    type Error = RevmStateProviderError;

    /// Gets basic account information.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.state_view
            .clone()
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
                    Some(match bytecode.kind() {
                        // A delegation designator is already exactly the 23-byte code; keep it as a
                        // 7702 bytecode so revm follows the delegation instead of executing
                        // `0xef01..` as legacy opcodes.
                        BytecodeKind::Eip7702 => bytecode,
                        // Legacy preimages still carry padding + artifacts; trim to the unpadded
                        // code the EVM actually runs.
                        BytecodeKind::LegacyAnalyzed => {
                            get_unpadded_code(bytecode.bytes_slice(), &props)
                        }
                    })
                };

                Ok(AccountInfo {
                    nonce: props.nonce,
                    balance: props.balance,
                    code_hash: observable_code_hash,
                    account_id: None,
                    code,
                })
            })
            .transpose()
    }

    /// Gets account code by its hash.
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .state_view
            .clone()
            .get_preimage(code_hash)
            .map(|bytes| bytecode_from_preimage(&bytes))
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
            .state_view
            .clone()
            .read(B256::from(flat_key.as_u8_array()))
            .unwrap_or_default()
            .into())
    }

    /// Gets block hash by block number.
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(diff) = self.state_block_number.checked_sub(number)
            && diff < 256
        {
            Ok(self.block_hashes.0[255 - diff as usize].into())
        } else {
            Ok(B256::default())
        }
    }
}
