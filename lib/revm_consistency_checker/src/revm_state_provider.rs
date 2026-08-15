use crate::helpers::get_unpadded_code;
use alloy::primitives::{Address, B256, KECCAK256_EMPTY};
use revm::DatabaseRef;
use revm::database_interface::DBErrorMarker;
use revm::primitives::{StorageKey, StorageValue};
use revm::state::{AccountInfo, Bytecode};
use ruint::aliases::B160;
use std::cell::RefCell;
use std::collections::HashMap;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_ee::utils::Bytes32;
use zksync_os_storage_api::{BlockHashes, ViewState};

fn fixed_bytes_to_bytes32(x: B256) -> Bytes32 {
    let x: [u8; 32] = x.into();
    x.into()
}

#[derive(Debug, Clone)]
pub struct RevmStateProvider<State>
where
    State: ViewState,
{
    state_view: State,
    block_hashes: BlockHashes,
    state_block_number: u64,
    /// Bytecodes keyed by keccak256(raw_code), populated during basic_ref.
    /// The deployer precompile looks up by keccak256 (observable hash),
    /// but the preimage DB stores by blake2s. This cache bridges the gap.
    keccak_code_cache: RefCell<HashMap<B256, Bytecode>>,
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
            keccak_code_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Preload a bytecode into the keccak256-keyed cache.
    ///
    /// Code minted in the very block being checked (force deployments in
    /// upgrade/genesis blocks) is looked up by the deployer precompile via
    /// its observable keccak256 hash, but the provider's pre-state view can
    /// never have cached it through an account read — the account does not
    /// exist yet.
    pub fn preload_code(&self, observable_code_hash: B256, code: Bytecode) {
        self.keccak_code_cache
            .borrow_mut()
            .insert(observable_code_hash, code);
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
                    // Load from preimage DB by blake2s hash, unpad to raw code.
                    let bytecode =
                        self.code_by_hash_ref(B256::from(props.bytecode_hash.as_u8_array()))?;
                    let raw = get_unpadded_code(bytecode.bytes_slice(), &props);
                    // Cache by keccak256 so the deployer precompile can look it up.
                    if !observable_code_hash.is_zero() && observable_code_hash != KECCAK256_EMPTY {
                        self.keccak_code_cache.borrow_mut().insert(
                            observable_code_hash,
                            Bytecode::new_raw(raw.original_bytes().clone()),
                        );
                    }
                    Some(raw)
                };

                let code_len = code.as_ref().map(|c| c.len()).unwrap_or(0);
                tracing::trace!("account({address}): nonce={} code_hash={observable_code_hash} code_len={code_len}", props.nonce);
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
    /// Checks keccak256 cache first (for deployer precompile lookups),
    /// then falls back to preimage DB (blake2s keying).
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(cached) = self.keccak_code_cache.borrow().get(&code_hash) {
            return Ok(cached.clone());
        }
        Ok(self
            .state_view
            .clone()
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
        let val = self
            .state_view
            .clone()
            .read(B256::from(flat_key.as_u8_array()))
            .unwrap_or_default();
        tracing::trace!("sload({address}, slot={index}) = {val}");
        Ok(val.into())
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
