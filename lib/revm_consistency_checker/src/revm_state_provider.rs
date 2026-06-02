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
use zksync_os_storage_api::{BlockHashes, ViewState};

fn fixed_bytes_to_bytes32(x: B256) -> Bytes32 {
    let x: [u8; 32] = x.into();
    x.into()
}

/// First three bytes of an EIP-7702 delegation designator: the `0xef01` magic followed by the
/// version byte `0x00`.
const EIP7702_DELEGATION_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];
/// Full length of an EIP-7702 delegation designator: 3-byte prefix + 20-byte address.
const EIP7702_DELEGATION_DESIGNATOR_LEN: usize = 23;

/// Build a revm [`Bytecode`] from a ZKsync OS preimage.
///
/// ZKsync OS stores an EIP-7702 delegation as the 23-byte designator `0xef0100 || address`
/// padded with trailing zeroes; revm's 7702 parser requires *exactly* 23 bytes, so the padding
/// is trimmed and the designator is parsed as a proper delegation (so revm follows it on call).
/// Regular contract bytecode (code + padding + artifacts) is wrapped verbatim.
fn bytecode_from_preimage(full_bytecode: &[u8]) -> Bytecode {
    if full_bytecode.len() >= EIP7702_DELEGATION_DESIGNATOR_LEN
        && full_bytecode.starts_with(&EIP7702_DELEGATION_PREFIX)
    {
        Bytecode::new_raw_checked(Bytes::copy_from_slice(
            &full_bytecode[..EIP7702_DELEGATION_DESIGNATOR_LEN],
        ))
        .expect("valid EIP-7702 delegation designator")
    } else {
        Bytecode::new_raw(Bytes::copy_from_slice(full_bytecode))
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
