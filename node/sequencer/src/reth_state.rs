use crate::CHAIN_ID;
use crate::repositories::RepositoryManager;
use crate::repositories::api_interface::ApiRepository;
use alloy::eips::{BlockNumHash, BlockNumberOrTag};
use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue};
use reth_chainspec::{Chain, ChainInfo, ChainSpec, ChainSpecBuilder, ChainSpecProvider};
use reth_primitives_traits::{Account, Bytecode};
use reth_revm::db::BundleState;
use reth_storage_api::errors::ProviderResult;
use reth_storage_api::{
    AccountReader, BlockHashReader, BlockIdReader, BlockNumReader, BytecodeReader,
    HashedPostStateProvider, StateProofProvider, StateProvider, StateProviderBox,
    StateProviderFactory, StateRootProvider, StorageRootProvider,
};
use reth_trie_common::updates::TrieUpdates;
use reth_trie_common::{
    AccountProof, HashedPostState, HashedStorage, MultiProof, MultiProofTargets, StorageMultiProof,
    StorageProof, TrieInput,
};
use std::fmt::Debug;
use std::sync::Arc;
use zk_ee::utils::Bytes32;
use zk_os_api::helpers::{get_balance, get_nonce};
use zk_os_forward_system::run::PreimageSource;
use zksync_os_state::{StateHandle, StateView};

#[derive(Debug)]
pub struct ZkClient {
    chain_spec: Arc<ChainSpec>,
    repositories: RepositoryManager,
    state_handle: StateHandle,
}

impl ZkClient {
    pub fn new(repositories: RepositoryManager, state_handle: StateHandle) -> Self {
        let builder = ChainSpecBuilder::default()
            .chain(Chain::from(CHAIN_ID))
            // Activate everything up to Cancun
            // TODO: Does it make sense to active Cancun if we do not support 4844 transactions?
            //       Maybe drop down to Shanghai?
            .cancun_activated()
            // TODO: Genesis is not used by the mempool but wouldn't hurt to provide the real one
            //       once we can
            .genesis(Default::default());
        Self {
            chain_spec: Arc::new(builder.build()),
            repositories,
            state_handle,
        }
    }
}

impl ChainSpecProvider for ZkClient {
    type ChainSpec = ChainSpec;

    fn chain_spec(&self) -> Arc<Self::ChainSpec> {
        self.chain_spec.clone()
    }
}

impl StateProviderFactory for ZkClient {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(ZkState {
            state_handle: self.state_handle.clone(),
            latest_block: self.repositories.get_latest_block(),
        }))
    }

    fn state_by_block_number_or_tag(
        &self,
        _number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        todo!()
    }

    fn history_by_block_number(&self, _block: BlockNumber) -> ProviderResult<StateProviderBox> {
        todo!()
    }

    fn history_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        todo!()
    }

    fn state_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        todo!()
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        todo!()
    }

    fn pending_state_by_hash(&self, _block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        todo!()
    }
}

#[derive(Debug)]
pub struct ZkState {
    state_handle: StateHandle,
    latest_block: u64,
}

impl ZkState {
    // I would like to just store a StateView but AccountReader and BytecodeReader
    // operate on an immutable reference, so they could use it concurrently.
    fn state_view(&self) -> StateView {
        self.state_handle
            .state_view_at_block(self.latest_block)
            .unwrap()
    }
}

impl AccountReader for ZkState {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self
            .state_view()
            .get_account(ruint::aliases::B160::from_le_bytes(address.into_array()))
            .map(|props| Account {
                nonce: get_nonce(&props),
                balance: get_balance(&props),
                bytecode_hash: Some(B256::from_slice(&props.bytecode_hash.as_u8_array())),
            }))
    }
}

impl BytecodeReader for ZkState {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(self
            .state_view()
            .get_preimage(Bytes32::from_array(
                code_hash.as_slice().try_into().unwrap(),
            ))
            .map(Bytes::from)
            .map(Bytecode::new_raw))
    }
}

//
//
// The rest of the file contains stub implementations purely to appease reth's type constraints.
// None of these methods are actually called by reth's mempool at runtime.
//
//

impl BlockHashReader for ZkState {
    fn block_hash(&self, _number: BlockNumber) -> ProviderResult<Option<B256>> {
        todo!()
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        todo!()
    }
}

impl StateRootProvider for ZkState {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        todo!()
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        todo!()
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        todo!()
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        todo!()
    }
}

impl StorageRootProvider for ZkState {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        todo!()
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        todo!()
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        todo!()
    }
}

impl StateProofProvider for ZkState {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        todo!()
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        todo!()
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        todo!()
    }
}

impl HashedPostStateProvider for ZkState {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        todo!()
    }
}

impl StateProvider for ZkState {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        todo!()
    }
}

impl BlockHashReader for ZkClient {
    fn block_hash(&self, _number: BlockNumber) -> ProviderResult<Option<B256>> {
        todo!()
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        todo!()
    }
}

impl BlockNumReader for ZkClient {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        todo!()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        todo!()
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        todo!()
    }

    fn block_number(&self, _hash: B256) -> ProviderResult<Option<BlockNumber>> {
        todo!()
    }
}

impl BlockIdReader for ZkClient {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        todo!()
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        todo!()
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        todo!()
    }
}
