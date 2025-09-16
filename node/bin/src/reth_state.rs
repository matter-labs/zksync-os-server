use alloy::eips::{BlockNumHash, BlockNumberOrTag};
use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue};
use reth_chainspec::{Chain, ChainInfo, ChainSpec, ChainSpecBuilder, ChainSpecProvider};
use reth_primitives_traits::{Account, Bytecode};
use reth_revm::db::BundleState;
use reth_storage_api::errors::{ProviderError, ProviderResult};
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
use zk_os_api::helpers::{get_balance, get_nonce};
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{ReadRepository, ReadStateHistory, ViewState};

#[derive(Debug)]
pub struct ZkClient<ReadState> {
    chain_spec: Arc<ChainSpec>,
    repositories: RepositoryManager,
    state_handle: ReadState,
}

impl<ReadState: ReadStateHistory> ZkClient<ReadState> {
    pub fn new(repositories: RepositoryManager, state_handle: ReadState, chain_id: u64) -> Self {
        let builder = ChainSpecBuilder::default()
            .chain(Chain::from(chain_id))
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

impl<ReadState: ReadStateHistory> ChainSpecProvider for ZkClient<ReadState> {
    type ChainSpec = ChainSpec;

    fn chain_spec(&self) -> Arc<Self::ChainSpec> {
        self.chain_spec.clone()
    }
}

impl<ReadState: ReadStateHistory + Clone> StateProviderFactory for ZkClient<ReadState> {
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

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        todo!()
    }
}

#[derive(Debug)]
pub struct ZkState<ReadState> {
    state_handle: ReadState,
    latest_block: u64,
}

impl<ReadState: ReadStateHistory> AccountReader for ZkState<ReadState> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self
            .state_handle
            .state_view_at(self.latest_block)
            .map_err(|_| ProviderError::StateAtBlockPruned(self.latest_block))?
            .get_account(*address)
            .map(|props| Account {
                nonce: get_nonce(&props),
                balance: get_balance(&props),
                bytecode_hash: if props.bytecode_hash.is_zero() {
                    None
                } else {
                    Some(B256::from_slice(&props.bytecode_hash.as_u8_array()))
                },
            }))
    }
}

impl<ReadStorage: ReadStateHistory> BytecodeReader for ZkState<ReadStorage> {
    fn bytecode_by_hash(&self, _code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        unimplemented!(
            "reth mempool only calls this for EIP-7702 transactions which we do not support yet"
        )
    }
}

//
//
// The rest of the file contains stub implementations purely to appease reth's type constraints.
// None of these methods are actually called by reth's mempool at runtime.
//
//

impl<ReadStorage: ReadStateHistory> BlockHashReader for ZkState<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> StateRootProvider for ZkState<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> StorageRootProvider for ZkState<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> StateProofProvider for ZkState<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> HashedPostStateProvider for ZkState<ReadStorage> {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        todo!()
    }
}

impl<ReadStorage: ReadStateHistory> StateProvider for ZkState<ReadStorage> {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        todo!()
    }
}

impl<ReadStorage: ReadStateHistory> BlockHashReader for ZkClient<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> BlockNumReader for ZkClient<ReadStorage> {
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

impl<ReadStorage: ReadStateHistory> BlockIdReader for ZkClient<ReadStorage> {
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
