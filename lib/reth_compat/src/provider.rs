use alloy::consensus::transaction::TransactionMeta;
use alloy::eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};
use alloy::genesis::{ChainConfig, Genesis};
use alloy::primitives::{
    Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, TxHash, TxNumber, U256,
    keccak256,
};
use reth_chainspec::{Chain, ChainInfo, ChainSpec, ChainSpecBuilder, ChainSpecProvider};
use reth_db_models::StoredBlockBodyIndices;
use reth_primitives::{Block as EthBlock, EthPrimitives, Receipt, TransactionSigned};
use reth_primitives_traits::{Account, Bytecode, RecoveredBlock, SealedHeader};
use reth_revm::db::BundleState;
use reth_storage_api::errors::any::AnyError;
use reth_storage_api::errors::{ProviderError, ProviderResult};
use reth_storage_api::{
    AccountReader, BlockBodyIndicesProvider, BlockHashReader, BlockIdReader, BlockNumReader,
    BlockReader, BlockReaderIdExt, BlockSource, BytecodeReader, HashedPostStateProvider,
    HeaderProvider, NodePrimitivesProvider, ReceiptProvider, ReceiptProviderIdExt,
    StateProofProvider, StateProvider, StateProviderBox, StateProviderFactory, StateRootProvider,
    StorageRootProvider, TransactionVariant, TransactionsProvider,
};
use reth_trie_common::updates::TrieUpdates;
use reth_trie_common::{
    AccountProof, HashedPostState, HashedStorage, MultiProof, MultiProofTargets, StorageMultiProof,
    StorageProof, TrieInput,
};
use std::fmt::Debug;
use std::ops::{RangeBounds, RangeInclusive};
use std::sync::Arc;
use zk_os_api::helpers::{get_balance, get_nonce};
use zksync_os_storage_api::{ReadRepository, ReadStateHistory, ViewState};

#[derive(Debug, Clone)]
pub struct ZkProviderFactory<State, Repository> {
    chain_spec: Arc<ChainSpec>,
    state: State,
    repository: Repository,
}

impl<State: ReadStateHistory, Repository: ReadRepository> ZkProviderFactory<State, Repository> {
    pub fn new(state: State, repository: Repository, chain_id: u64) -> Self {
        let (genesis, genesis_hash) = repository
            .get_block_by_number(0)
            .expect("failed to read repository")
            .expect("genesis missing from repository")
            .into_parts();
        let builder = ChainSpecBuilder::default()
            .chain(Chain::from(chain_id))
            // Activate everything up to Cancun
            // todo: does it make sense to active Cancun if we do not support 4844 transactions?
            //       maybe drop down to Shanghai?
            .cancun_activated()
            .genesis(Genesis {
                // todo: evaluate whether genesis config needs to be tweaked
                config: ChainConfig::default(),
                // todo: does not seem to be used in zksync-os; tbc
                nonce: 0,
                timestamp: genesis.timestamp,
                // todo: does not seem to be used in zksync-os; tbc
                extra_data: Default::default(),
                gas_limit: genesis.gas_limit,
                difficulty: genesis.difficulty,
                mix_hash: genesis.mix_hash,
                coinbase: genesis.beneficiary,
                // todo: set real initial account state
                alloc: Default::default(),
                base_fee_per_gas: None,
                excess_blob_gas: None,
                blob_gas_used: None,
                number: None,
                parent_hash: None,
            });
        let mut chain_spec = builder.build();
        // Patch genesis header to take chain id into account. Needed to give our chains different
        // genesis hash that is otherwise identical.
        let genesis_hash = keccak256([genesis_hash, B256::from(U256::from(chain_id))].concat());
        chain_spec.genesis_header = SealedHeader::new(genesis.header, genesis_hash);
        Self {
            chain_spec: Arc::new(chain_spec),
            state,
            repository,
        }
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> ChainSpecProvider
    for ZkProviderFactory<State, Repository>
{
    type ChainSpec = ChainSpec;

    fn chain_spec(&self) -> Arc<Self::ChainSpec> {
        self.chain_spec.clone()
    }
}

impl<State: ReadStateHistory + Clone, Repository: ReadRepository + Clone> StateProviderFactory
    for ZkProviderFactory<State, Repository>
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(ZkProvider {
            state: self.state.clone(),
            latest_block: self.repository.get_latest_block(),
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
pub(crate) struct ZkProvider<State> {
    state: State,
    latest_block: u64,
}

impl<State: ReadStateHistory> AccountReader for ZkProvider<State> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self
            .state
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

impl<ReadStorage: ReadStateHistory> BytecodeReader for ZkProvider<ReadStorage> {
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

impl<State: ReadStateHistory> BlockHashReader for ZkProvider<State> {
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

impl<State: ReadStateHistory> StateRootProvider for ZkProvider<State> {
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

impl<State: ReadStateHistory> StorageRootProvider for ZkProvider<State> {
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

impl<State: ReadStateHistory> StateProofProvider for ZkProvider<State> {
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

impl<State: ReadStateHistory> HashedPostStateProvider for ZkProvider<State> {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        todo!()
    }
}

impl<State: ReadStateHistory> StateProvider for ZkProvider<State> {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        todo!()
    }

    fn storage_by_hashed_key(
        &self,
        _address: Address,
        _hashed_storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        todo!()
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> BlockHashReader
    for ZkProviderFactory<State, Repository>
{
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

impl<State: ReadStateHistory, Repository: ReadRepository> BlockNumReader
    for ZkProviderFactory<State, Repository>
{
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        todo!()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self.repository.get_latest_block())
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self.repository.get_latest_block())
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        Ok(self
            .repository
            .get_block_by_hash(hash)
            .map_err(AnyError::new)?
            .map(|b| b.number))
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> BlockIdReader
    for ZkProviderFactory<State, Repository>
{
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

impl<State: ReadStateHistory, Repository: ReadRepository> HeaderProvider
    for ZkProviderFactory<State, Repository>
{
    type Header = alloy::consensus::Header;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        Ok(self
            .repository
            .get_block_by_hash(block_hash)
            .map_err(AnyError::new)?
            .map(|b| b.header.clone()))
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Self::Header>> {
        Ok(self
            .repository
            .get_block_by_number(num)
            .map_err(AnyError::new)?
            .map(|b| b.header.clone()))
    }

    fn headers_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        Ok(Vec::new())
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        Ok(self
            .repository
            .get_block_by_number(number)
            .map_err(AnyError::new)?
            .map(|b| SealedHeader::new(b.header.clone(), b.hash())))
    }

    fn sealed_headers_while(
        &self,
        _range: impl RangeBounds<BlockNumber>,
        _predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        Ok(Vec::new())
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> NodePrimitivesProvider
    for ZkProviderFactory<State, Repository>
{
    type Primitives = EthPrimitives;
}

impl<State: ReadStateHistory, Repository: ReadRepository> TransactionsProvider
    for ZkProviderFactory<State, Repository>
{
    type Transaction = TransactionSigned;

    fn transaction_id(&self, _tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        Ok(None)
    }

    fn transaction_by_id(&self, _id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_id_unhashed(
        &self,
        _id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_hash_with_meta(
        &self,
        _hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        Ok(None)
    }

    fn transactions_by_block(
        &self,
        _block_id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        Ok(Vec::new())
    }

    fn transactions_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        Ok(Vec::new())
    }

    fn senders_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        Ok(Vec::new())
    }

    fn transaction_sender(&self, _id: TxNumber) -> ProviderResult<Option<Address>> {
        Ok(None)
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> ReceiptProvider
    for ZkProviderFactory<State, Repository>
{
    type Receipt = Receipt;

    fn receipt(&self, _id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        Ok(None)
    }

    fn receipt_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        Ok(None)
    }

    fn receipts_by_block(
        &self,
        _block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        Ok(None)
    }

    fn receipts_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        Ok(Vec::new())
    }

    fn receipts_by_block_range(
        &self,
        _block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        Ok(Vec::new())
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> ReceiptProviderIdExt
    for ZkProviderFactory<State, Repository>
{
}

impl<State: ReadStateHistory, Repository: ReadRepository> BlockBodyIndicesProvider
    for ZkProviderFactory<State, Repository>
{
    fn block_body_indices(&self, _num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        Ok(None)
    }

    fn block_body_indices_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        Ok(Vec::new())
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> BlockReader
    for ZkProviderFactory<State, Repository>
{
    type Block = EthBlock;

    fn find_block_by_hash(
        &self,
        _hash: B256,
        _source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        Ok(None)
    }

    fn block(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        Ok(None)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock<Self::Block>, Vec<Self::Receipt>)>> {
        Ok(None)
    }

    fn recovered_block(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn sealed_block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn block_range(&self, _range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        Ok(Vec::new())
    }

    fn block_with_senders_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        Ok(Vec::new())
    }

    fn recovered_block_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        Ok(Vec::new())
    }

    fn block_by_transaction_id(&self, _id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl<State: ReadStateHistory, Repository: ReadRepository> BlockReaderIdExt
    for ZkProviderFactory<State, Repository>
{
    fn block_by_id(&self, _id: BlockId) -> ProviderResult<Option<Self::Block>> {
        Ok(None)
    }

    fn sealed_header_by_id(
        &self,
        id: BlockId,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        match id {
            BlockId::Number(num) => match self.convert_block_number(num)? {
                Some(n) => self.sealed_header(n),
                None => Ok(None),
            },
            BlockId::Hash(hash) => Ok(self
                .repository
                .get_block_by_hash(hash.block_hash)
                .map_err(AnyError::new)?
                .map(|b| SealedHeader::new(b.header.clone(), b.hash()))),
        }
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Header>> {
        match id {
            BlockId::Number(num) => match self.convert_block_number(num)? {
                Some(n) => self.header_by_number(n),
                None => Ok(None),
            },
            BlockId::Hash(hash) => self.header(hash.block_hash),
        }
    }
}
