use crate::api::eth_call_handler::EthCallHandler;
use crate::api::metrics::API_METRICS;
use crate::api::result::{internal_rpc_err, unimplemented_rpc_err, ToRpcResult};
use crate::api::tx_handler::TxHandler;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::repositories::api_interface::{ApiRepository, ApiRepositoryExt, RepositoryError};
use crate::repositories::transaction_receipt_repository::TxMeta;
use crate::reth_state::ZkClient;
use crate::CHAIN_ID;
use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::consensus::Account;
use alloy::dyn_abi::TypedData;
use alloy::eips::eip2930::AccessListResult;
use alloy::eips::{BlockId, BlockNumberOrTag, Encodable2718};
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, BlockNumber, Bytes, TxHash, B256, U256, U64};
use alloy::rpc::types::simulate::{SimulatePayload, SimulatedBlock};
use alloy::rpc::types::state::StateOverride;
use alloy::rpc::types::{
    AccountInfo, Block, BlockOverrides, Bundle, EIP1186AccountProofResponse, EthCallResponse,
    FeeHistory, Header, Index, Log, StateContext, SyncStatus, Transaction, TransactionReceipt,
    TransactionRequest,
};
use alloy::serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::ReadStorage;
use zksync_os_mempool::{RethPool, RethTransactionPool};
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_state::StateHandle;
use zksync_os_types::{L2Envelope, ZkEnvelope};

pub(crate) struct EthNamespace<R> {
    tx_handler: TxHandler,
    eth_call_handler: EthCallHandler<R>,

    // todo: the idea is to only have handlers here, but then get_balance would require its own handler
    // reconsider approach to API in this regard
    pub(super) repository: R,
    mempool: RethPool<ZkClient>,
    state_handle: StateHandle,

    pub(super) chain_id: u64,
}

impl<R: ApiRepository + Clone> EthNamespace<R> {
    pub fn new(
        config: RpcConfig,

        repository: R,
        state_handle: StateHandle,
        mempool: RethPool<ZkClient>,
        block_replay_storage: BlockReplayStorage,
    ) -> Self {
        let tx_handler = TxHandler::new(mempool.clone());

        let eth_call_handler = EthCallHandler::new(
            config,
            state_handle.clone(),
            block_replay_storage,
            repository.clone(),
        );
        Self {
            tx_handler,
            eth_call_handler,
            repository,
            mempool,
            state_handle,
            chain_id: CHAIN_ID,
        }
    }
}

impl<R: ApiRepository> EthNamespace<R> {
    fn block_number_impl(&self) -> EthResult<U256> {
        Ok(U256::from(self.repository.get_canonized_block()))
    }

    fn block_by_id_impl(
        &self,
        block_id: Option<BlockId>,
        full: bool,
    ) -> EthResult<Option<Block<ZkEnvelope>>> {
        let block_id = block_id.unwrap_or_default();
        let Some(block) = self.repository.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        if full {
            let (block, hash) = block.into_parts();
            let full_transactions = block
                .body
                .transactions
                .into_iter()
                .map(|tx_hash| {
                    self.repository
                        .get_transaction(tx_hash)?
                        .ok_or(EthError::BlockNotFound(block_id))
                        .map(|tx| tx.into_envelope())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(Block::new(
                Header::from_consensus(block.header.seal(hash), None, None),
                BlockTransactions::Full(full_transactions),
            )))
        } else {
            let hash = block.hash();
            let block = block.unseal();
            Ok(Some(Block::new(
                Header::from_consensus(block.header.seal(hash), None, None),
                BlockTransactions::Hashes(block.body.transactions),
            )))
        }
    }

    fn block_transaction_count_by_id_impl(&self, block_id: BlockId) -> EthResult<Option<U256>> {
        let Some(block) = self.repository.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        Ok(Some(U256::from(block.body.transactions.len())))
    }

    fn block_receipts_impl(&self, block_id: BlockId) -> EthResult<Option<Vec<TransactionReceipt>>> {
        let Some(block) = self.repository.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let mut receipts = Vec::new();
        for tx_hash in block.unseal().body.transactions {
            let Some(rpc_receipt) = self.transaction_receipt_impl(tx_hash)? else {
                return Ok(None);
            };
            receipts.push(rpc_receipt);
        }
        Ok(Some(receipts))
    }

    fn block_uncles_count_by_id_impl(&self, block_id: BlockId) -> EthResult<Option<U256>> {
        let block = self.repository.get_block_by_id(block_id)?;
        if block.is_some() {
            // ZKsync OS is not PoW and hence does not have uncle blocks
            Ok(Some(U256::ZERO))
        } else {
            Ok(None)
        }
    }

    fn raw_transaction_by_hash_impl(&self, hash: B256) -> EthResult<Option<Bytes>> {
        // Look up in repositories first to avoid race condition
        if let Some(raw_tx) = self.repository.get_raw_transaction(hash)? {
            return Ok(Some(Bytes::from(raw_tx)));
        }
        if let Some(pool_tx) = self.mempool.get(&hash) {
            return Ok(Some(Bytes::from(
                pool_tx.transaction.transaction.encoded_2718(),
            )));
        }
        Ok(None)
    }

    fn transaction_by_hash_impl(&self, hash: B256) -> EthResult<Option<Transaction<ZkEnvelope>>> {
        if let Some(tx) = self.repository.get_transaction(hash)? {
            if let Some(meta) = self.repository.get_transaction_meta(hash)? {
                return Ok(Some(Transaction::from_transaction(
                    tx.inner,
                    TransactionInfo {
                        hash: Some(hash),
                        index: Some(meta.tx_index_in_block),
                        block_hash: Some(meta.block_hash),
                        block_number: Some(meta.block_number),
                        base_fee: Some(meta.effective_gas_price as u64),
                    },
                )));
            }
        }
        if let Some(pool_tx) = self.mempool.get(&hash) {
            let envelope = L2Envelope::from(pool_tx.transaction.transaction.inner().clone());
            return Ok(Some(Transaction::from_transaction(
                Recovered::new_unchecked(
                    ZkEnvelope::L2(envelope),
                    pool_tx.transaction.transaction.signer(),
                ),
                TransactionInfo {
                    hash: None,
                    index: None,
                    block_hash: None,
                    block_number: None,
                    base_fee: None,
                },
            )));
        }
        Ok(None)
    }

    fn raw_transaction_by_block_id_and_index_impl(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> EthResult<Option<Bytes>> {
        let Some(block) = self.repository.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let Some(tx_hash) = block.body.transactions.get(index.0) else {
            return Ok(None);
        };
        Ok(self
            .repository
            .get_raw_transaction(*tx_hash)?
            .map(Bytes::from))
    }

    fn transaction_by_block_id_and_index_impl(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> EthResult<Option<Transaction<ZkEnvelope>>> {
        let Some(block) = self.repository.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let Some(tx_hash) = block.body.transactions.get(index.0) else {
            return Ok(None);
        };
        let Some(tx) = self.repository.get_transaction(*tx_hash)? else {
            return Ok(None);
        };
        let Some(meta) = self.repository.get_transaction_meta(*tx_hash)? else {
            return Ok(None);
        };
        Ok(Some(Transaction::from_transaction(
            tx.inner,
            TransactionInfo {
                hash: Some(*tx_hash),
                index: Some(meta.tx_index_in_block),
                block_hash: Some(meta.block_hash),
                block_number: Some(meta.block_number),
                base_fee: block.base_fee_per_gas,
            },
        )))
    }

    fn transaction_by_sender_and_nonce_impl(
        &self,
        sender: Address,
        nonce: U64,
    ) -> EthResult<Option<Transaction<ZkEnvelope>>> {
        let Some(tx_hash) = self
            .repository
            .get_transaction_hash_by_sender_nonce(sender, nonce.saturating_to())?
        else {
            return Ok(None);
        };
        self.transaction_by_hash_impl(tx_hash)
    }

    fn transaction_receipt_impl(&self, tx_hash: B256) -> EthResult<Option<TransactionReceipt>> {
        let Some(stored_tx) = self.repository.get_stored_transaction(tx_hash)? else {
            return Ok(None);
        };
        let mut log_index_in_tx = 0;
        let inner_receipt = stored_tx.receipt.map_logs(|inner_log| {
            let log = build_api_log(tx_hash, inner_log, stored_tx.meta, log_index_in_tx);
            log_index_in_tx += 1;
            log
        });
        Ok(Some(TransactionReceipt {
            inner: inner_receipt,
            transaction_hash: tx_hash,
            transaction_index: Some(stored_tx.meta.tx_index_in_block),
            block_hash: Some(stored_tx.meta.block_hash),
            block_number: Some(stored_tx.meta.block_number),
            gas_used: stored_tx.meta.gas_used,
            effective_gas_price: stored_tx.meta.effective_gas_price,
            blob_gas_used: None,
            blob_gas_price: None,
            from: stored_tx.tx.signer(),
            to: stored_tx.tx.to(),
            contract_address: stored_tx.meta.contract_address,
        }))
    }

    fn balance_impl(&self, address: Address, block_id: Option<BlockId>) -> EthResult<U256> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.repository.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };
        Ok(self
            .repository
            .account_property_repository()
            .get_at_block(block_number, &address)
            .map(|props| props.balance)
            .unwrap_or(U256::ZERO))
    }

    fn storage_at_impl(
        &self,
        address: Address,
        key: JsonStorageKey,
        block_id: Option<BlockId>,
    ) -> EthResult<B256> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.repository.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        let flat_key = derive_flat_storage_key(
            &B160::from_be_bytes(address.into_array()),
            &Bytes32::from(key.as_b256().0),
        );
        let Ok(mut state) = self.state_handle.state_view_at_block(block_number) else {
            return Err(EthError::BlockStateNotAvailable(block_number));
        };
        Ok(B256::from(
            state.read(flat_key).unwrap_or_default().as_u8_array(),
        ))
    }

    fn transaction_count_impl(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> EthResult<U256> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.repository.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        // todo(#36): distinguish between N/A blocks and actual missing accounts
        let nonce = self
            .repository
            .account_property_repository()
            .get_at_block(block_number, &address)
            .map(|props| props.nonce)
            .unwrap_or(0);
        Ok(U256::from(nonce))
    }

    fn get_code_impl(&self, address: Address, block_id: Option<BlockId>) -> EthResult<Bytes> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.repository.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        // todo(#36): distinguish between N/A blocks and actual missing accounts
        let Some(props) = self
            .repository
            .account_property_repository()
            .get_at_block(block_number, &address)
        else {
            return Ok(Bytes::default());
        };
        let bytecode_hash = B256::from(props.bytecode_hash.as_u8_array());
        Ok(Bytes::from(
            self.repository
                .bytecode_repository()
                .get_at_block(block_number, &bytecode_hash)
                .unwrap_or_default(),
        ))
    }
}

#[async_trait]
impl<R: ApiRepository + 'static> EthApiServer for EthNamespace<R> {
    async fn protocol_version(&self) -> RpcResult<String> {
        Ok("zksync_os/0.0.1".to_string())
    }

    fn syncing(&self) -> RpcResult<SyncStatus> {
        // We do not have decentralization yet, so the node is always synced
        // todo: report sync status once we have consensus integrated
        Ok(SyncStatus::None)
    }

    async fn author(&self) -> RpcResult<Address> {
        // Author aka coinbase aka etherbase is the account where mining profits are credited to.
        // As ZKsync OS is not PoW we do not implement this method.
        Err(unimplemented_rpc_err())
    }

    fn accounts(&self) -> RpcResult<Vec<Address>> {
        // ZKsync OS node never manages local accounts (i.e., accounts available for signing on the
        // node's side).
        Ok(Vec::new())
    }

    fn block_number(&self) -> RpcResult<U256> {
        self.block_number_impl().to_rpc_result()
    }

    async fn chain_id(&self) -> RpcResult<Option<U64>> {
        Ok(Some(U64::from(self.chain_id)))
    }

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<Block<ZkEnvelope>>> {
        self.block_by_id_impl(Some(hash.into()), full)
            .to_rpc_result()
    }

    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<Block<ZkEnvelope>>> {
        self.block_by_id_impl(Some(number.into()), full)
            .to_rpc_result()
    }

    async fn block_transaction_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>> {
        self.block_transaction_count_by_id_impl(hash.into())
            .to_rpc_result()
    }

    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        self.block_transaction_count_by_id_impl(number.into())
            .to_rpc_result()
    }

    async fn block_uncles_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>> {
        self.block_uncles_count_by_id_impl(hash.into())
            .to_rpc_result()
    }

    async fn block_uncles_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        self.block_uncles_count_by_id_impl(number.into())
            .to_rpc_result()
    }

    async fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Option<Vec<TransactionReceipt>>> {
        self.block_receipts_impl(block_id).to_rpc_result()
    }

    async fn uncle_by_block_hash_and_index(
        &self,
        _hash: B256,
        _index: Index,
    ) -> RpcResult<Option<Block>> {
        // ZKsync OS is not PoW and hence does not have uncle blocks
        Ok(None)
    }

    async fn uncle_by_block_number_and_index(
        &self,
        _number: BlockNumberOrTag,
        _index: Index,
    ) -> RpcResult<Option<Block>> {
        // ZKsync OS is not PoW and hence does not have uncle blocks
        Ok(None)
    }

    async fn raw_transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Bytes>> {
        self.raw_transaction_by_hash_impl(hash).to_rpc_result()
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction<ZkEnvelope>>> {
        self.transaction_by_hash_impl(hash).to_rpc_result()
    }

    async fn raw_transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> RpcResult<Option<Bytes>> {
        self.raw_transaction_by_block_id_and_index_impl(hash.into(), index)
            .to_rpc_result()
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> RpcResult<Option<Transaction<ZkEnvelope>>> {
        self.transaction_by_block_id_and_index_impl(hash.into(), index)
            .to_rpc_result()
    }

    async fn raw_transaction_by_block_number_and_index(
        &self,
        number: BlockNumberOrTag,
        index: Index,
    ) -> RpcResult<Option<Bytes>> {
        self.raw_transaction_by_block_id_and_index_impl(number.into(), index)
            .to_rpc_result()
    }

    async fn transaction_by_block_number_and_index(
        &self,
        number: BlockNumberOrTag,
        index: Index,
    ) -> RpcResult<Option<Transaction<ZkEnvelope>>> {
        self.transaction_by_block_id_and_index_impl(number.into(), index)
            .to_rpc_result()
    }

    async fn transaction_by_sender_and_nonce(
        &self,
        address: Address,
        nonce: U64,
    ) -> RpcResult<Option<Transaction<ZkEnvelope>>> {
        self.transaction_by_sender_and_nonce_impl(address, nonce)
            .to_rpc_result()
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<TransactionReceipt>> {
        self.transaction_receipt_impl(hash).to_rpc_result()
    }

    async fn balance(&self, address: Address, block_id: Option<BlockId>) -> RpcResult<U256> {
        self.balance_impl(address, block_id).to_rpc_result()
    }

    async fn storage_at(
        &self,
        address: Address,
        key: JsonStorageKey,
        block_id: Option<BlockId>,
    ) -> RpcResult<B256> {
        self.storage_at_impl(address, key, block_id).to_rpc_result()
    }

    async fn transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> RpcResult<U256> {
        self.transaction_count_impl(address, block_id)
            .to_rpc_result()
    }

    async fn get_code(&self, address: Address, block_id: Option<BlockId>) -> RpcResult<Bytes> {
        self.get_code_impl(address, block_id).to_rpc_result()
    }

    async fn header_by_number(&self, block_number: BlockNumberOrTag) -> RpcResult<Option<Header>> {
        Ok(self
            .block_by_id_impl(Some(block_number.into()), false)
            .to_rpc_result()?
            .map(|block| block.header))
    }

    async fn header_by_hash(&self, hash: B256) -> RpcResult<Option<Header>> {
        Ok(self
            .block_by_id_impl(Some(hash.into()), false)
            .to_rpc_result()?
            .map(|block| block.header))
    }

    async fn simulate_v1(
        &self,
        _opts: SimulatePayload,
        _block_number: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock>> {
        // todo(#51): implement
        Err(unimplemented_rpc_err())
    }

    async fn call(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        let latency = API_METRICS.response_time[&"call"].start();
        let r = self
            .eth_call_handler
            .call_impl(request, block_number, state_overrides, block_overrides)
            .to_rpc_result();
        latency.observe();
        r
    }

    async fn call_many(
        &self,
        _bundles: Vec<Bundle>,
        _state_context: Option<StateContext>,
        _state_override: Option<StateOverride>,
    ) -> RpcResult<Vec<Vec<EthCallResponse>>> {
        // todo(#52): implement
        Err(unimplemented_rpc_err())
    }

    async fn create_access_list(
        &self,
        _request: TransactionRequest,
        _block_number: Option<BlockId>,
        _state_override: Option<StateOverride>,
    ) -> RpcResult<AccessListResult> {
        // todo(EIP-7702)
        Err(unimplemented_rpc_err())
    }

    async fn estimate_gas(
        &self,
        _request: TransactionRequest,
        _block_number: Option<BlockId>,
        _state_override: Option<StateOverride>,
    ) -> RpcResult<U256> {
        // todo(#26): real implementation
        let latency = API_METRICS.response_time[&"estimate_gas"].start();
        latency.observe();
        Ok(U256::from(1000000))
    }

    async fn gas_price(&self) -> RpcResult<U256> {
        // todo(#??): real implementation
        Ok(U256::from(1000))
    }

    async fn get_account(&self, _address: Address, _block: BlockId) -> RpcResult<Option<Account>> {
        // todo(#36): implement
        Err(unimplemented_rpc_err())
    }

    async fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
        // todo(#??): real implementation
        Ok(U256::from(0))
    }

    async fn blob_base_fee(&self) -> RpcResult<U256> {
        // todo(EIP-4844)
        Err(unimplemented_rpc_err())
    }

    async fn fee_history(
        &self,
        block_count: U64,
        _newest_block: BlockNumberOrTag,
        _reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<FeeHistory> {
        // todo(#??): real implementation
        let block_count: usize = block_count.try_into().unwrap();
        Ok(FeeHistory {
            base_fee_per_gas: vec![10000u128; block_count],
            gas_used_ratio: vec![0.5; block_count],
            base_fee_per_blob_gas: vec![],
            blob_gas_used_ratio: vec![],
            oldest_block: 0,
            reward: None,
        })
    }

    async fn send_transaction(&self, _request: TransactionRequest) -> RpcResult<B256> {
        Err(internal_rpc_err("node has no signer accounts"))
    }

    async fn send_raw_transaction(&self, bytes: Bytes) -> RpcResult<B256> {
        let latency = API_METRICS.response_time[&"send_raw_transaction"].start();

        let r = self
            .tx_handler
            .send_raw_transaction_impl(bytes)
            .await
            .to_rpc_result();
        latency.observe();

        r
    }

    async fn sign(&self, _address: Address, _message: Bytes) -> RpcResult<Bytes> {
        Err(internal_rpc_err("node has no signer accounts"))
    }

    async fn sign_transaction(&self, _transaction: TransactionRequest) -> RpcResult<Bytes> {
        Err(internal_rpc_err("node has no signer accounts"))
    }

    async fn sign_typed_data(&self, _address: Address, _data: TypedData) -> RpcResult<Bytes> {
        Err(internal_rpc_err("node has no signer accounts"))
    }

    async fn get_proof(
        &self,
        _address: Address,
        _keys: Vec<JsonStorageKey>,
        _block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        Err(internal_rpc_err(
            "unsupported as ZKsync OS has a different storage layout than Ethereum",
        ))
    }

    async fn get_account_info(&self, _address: Address, _block: BlockId) -> RpcResult<AccountInfo> {
        // todo(#36): implement
        Err(unimplemented_rpc_err())
    }
}

pub(super) fn build_api_log(
    tx_hash: TxHash,
    primitive_log: alloy::primitives::Log,
    tx_meta: TxMeta,
    log_index_in_tx: u64,
) -> Log {
    Log {
        inner: primitive_log,
        block_hash: Some(tx_meta.block_hash),
        block_number: Some(tx_meta.block_number),
        block_timestamp: Some(tx_meta.block_timestamp),
        transaction_hash: Some(tx_hash),
        transaction_index: Some(tx_meta.tx_index_in_block),
        log_index: Some(tx_meta.number_of_logs_before_this_tx + log_index_in_tx),
        removed: false,
    }
}

/// `eth` namespace result type.
pub type EthResult<Ok> = Result<Ok, EthError>;

/// General `eth` namespace errors
#[derive(Debug, thiserror::Error)]
pub enum EthError {
    /// Block could not be found by its id (hash/number/tag).
    #[error("block not found")]
    BlockNotFound(BlockId),
    // todo: consider moving this to `zksync_os_state` crate
    /// Block has been compacted.
    #[error("state for block {0} is not available")]
    BlockStateNotAvailable(BlockNumber),

    #[error(transparent)]
    RepositoryError(#[from] RepositoryError),
}
