use crate::CHAIN_ID;
use async_trait::async_trait;
use zksync_web3_decl::jsonrpsee::core::RpcResult;
use zksync_web3_decl::types::{
    Block, Bytes, Filter, FilterChanges, Index, Log, SyncState, TransactionReceipt, U64Number,
    U256, U64,
};

use zksync_web3_decl::namespaces::EthNamespaceServer;

use crate::api::eth_call_handler::EthCallHandler;
use crate::api::metrics::API_METRICS;
use crate::api::resolve_block_id;
use crate::api::tx_handler::TxHandler;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::conversions::{h256_to_bytes32, ruint_u256_to_api_u256};
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use zksync_os_mempool::DynPool;
use zksync_os_state::StateHandle;
use zksync_types::{
    api::{
        state_override::StateOverride, BlockId, BlockIdVariant, BlockNumber, FeeHistory,
        Transaction, TransactionVariant,
    },
    transaction_request::CallRequest,
    Address, H256,
};
use zksync_web3_decl::jsonrpsee::types::error::INTERNAL_ERROR_CODE;
use zksync_web3_decl::jsonrpsee::types::ErrorObject;

pub(crate) struct EthNamespace {
    tx_handler: TxHandler,
    eth_call_handler: EthCallHandler,

    // todo: the idea is to only have handlers here, but then get_balance would require its own handler
    // reconsider approach to API in this regard
    repository_manager: RepositoryManager,

    finality_info: FinalityTracker,
}

impl EthNamespace {
    pub fn map_err(&self, err: anyhow::Error) -> ErrorObject<'static> {
        tracing::warn!("Error in EthNamespace: {}", err);
        ErrorObject::owned(INTERNAL_ERROR_CODE, err.to_string(), None::<()>)
    }

    pub fn new(
        config: RpcConfig,

        repository_manager: RepositoryManager,
        finality_tracker: FinalityTracker,
        state_handle: StateHandle,
        mempool: DynPool,
        block_replay_storage: BlockReplayStorage,
    ) -> EthNamespace {
        let tx_handler = TxHandler::new(
            mempool,
            repository_manager.account_property_repository.clone(),
            config.max_nonce_ahead,
            config.max_tx_size_bytes,
        );

        let guarded_state = finality_tracker.canonized_state_guard(state_handle.clone());
        let eth_call_handler = EthCallHandler::new(
            config,
            finality_tracker.clone(),
            guarded_state,
            block_replay_storage,
        );
        Self {
            tx_handler,
            eth_call_handler,
            repository_manager,
            finality_info: finality_tracker,
        }
    }
}

#[async_trait]
impl EthNamespaceServer for EthNamespace {
    // todo: temporary solution for EN
    // async fn block_replay(&self, block_number: u64) -> RpcResult<Value> {
    //     let Some(replay_record) = self
    //         .block_replay_storage
    //         .get_replay_record(block_number)
    //     else {
    //         return Ok(Value::Null);
    //     };
    //
    //     Ok(serde_json::to_value(replay_record).unwrap())
    // }

    async fn get_block_number(&self) -> RpcResult<U64> {
        // todo (Daniyar): really add plus one?
        let res = self.finality_info.get_canonized_block() + 1;
        // tracing::debug!("get_block_number: res: {:?}", res);
        Ok(res.into())
    }

    async fn chain_id(&self) -> RpcResult<U64> {
        Ok(CHAIN_ID.into())
    }

    async fn call(
        &self,
        req: CallRequest,
        block: Option<BlockIdVariant>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<Bytes> {
        let latency = API_METRICS.response_time[&"call"].start();
        let r = self
            .eth_call_handler
            .call_impl(req, block, state_override)
            .map_err(|err| self.map_err(err));
        latency.observe();
        r
    }

    async fn estimate_gas(
        &self,
        _req: CallRequest,
        _block: Option<BlockNumber>,
        _state_override: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let latency = API_METRICS.response_time[&"estimate_gas"].start();
        latency.observe();
        Ok(U256::from("1000000"))
    }

    async fn gas_price(&self) -> RpcResult<U256> {
        Ok(U256::from(1000))
    }

    async fn new_filter(&self, _filter: Filter) -> RpcResult<U256> {
        unimplemented!()
    }

    async fn new_block_filter(&self) -> RpcResult<U256> {
        unimplemented!()
    }

    async fn uninstall_filter(&self, _idx: U256) -> RpcResult<bool> {
        unimplemented!()
    }

    async fn new_pending_transaction_filter(&self) -> RpcResult<U256> {
        unimplemented!()
    }

    async fn get_logs(&self, _filter: Filter) -> RpcResult<Vec<Log>> {
        unimplemented!()
    }

    async fn get_filter_logs(&self, _filter_index: U256) -> RpcResult<FilterChanges> {
        unimplemented!()
    }

    async fn get_filter_changes(&self, _filter_index: U256) -> RpcResult<FilterChanges> {
        unimplemented!()
    }

    async fn get_balance(
        &self,
        address: Address,
        block: Option<BlockIdVariant>,
    ) -> RpcResult<U256> {
        //todo Daniyar: really add +1?
        let block_number = resolve_block_id(block, &self.finality_info) + 1;
        let balance = self
            .repository_manager
            .account_property_repository
            .get_at_block(block_number, &address)
            .map(|props| ruint_u256_to_api_u256(props.balance))
            .unwrap_or(U256::zero());

        // tracing::info!(
        //     "get_balance: address: {:?}, block: {:?}, balance: {:?}",
        //     address,
        //     block,
        //     balance
        // );

        Ok(balance)
    }

    async fn get_block_by_number(
        &self,
        _block_number: BlockNumber,
        _full_transactions: bool,
    ) -> RpcResult<Option<Block<TransactionVariant>>> {
        unimplemented!()
    }

    async fn get_block_by_hash(
        &self,
        _hash: H256,
        _full_transactions: bool,
    ) -> RpcResult<Option<Block<TransactionVariant>>> {
        unimplemented!()
    }

    async fn get_block_transaction_count_by_number(
        &self,
        _block_number: BlockNumber,
    ) -> RpcResult<Option<U256>> {
        unimplemented!()
    }

    async fn get_block_receipts(
        &self,
        _block_id: BlockId,
    ) -> RpcResult<Option<Vec<TransactionReceipt>>> {
        unimplemented!()
    }

    async fn get_block_transaction_count_by_hash(
        &self,
        _block_hash: H256,
    ) -> RpcResult<Option<U256>> {
        unimplemented!()
    }

    async fn get_code(
        &self,
        _address: Address,
        _block: Option<BlockIdVariant>,
    ) -> RpcResult<Bytes> {
        unimplemented!()
    }

    async fn get_storage_at(
        &self,
        _address: Address,
        _idx: U256,
        _block: Option<BlockIdVariant>,
    ) -> RpcResult<H256> {
        unimplemented!()
    }

    async fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockIdVariant>,
    ) -> RpcResult<U256> {
        // returning nonce from ethemeral txs
        let nonce = self
            .repository_manager
            .account_property_repository
            .get_latest(&address)
            .map(|props| props.nonce)
            .unwrap_or(0);

        let resolved_block = resolve_block_id(block, &self.finality_info) + 1;

        tracing::info!(
            address=?address,
            block=?block,
            resolved_block=resolved_block,
            nonce=nonce,
            "get_transaction_count resolved",
        );

        Ok(nonce.into())
    }

    async fn get_transaction_by_hash(&self, hash: H256) -> RpcResult<Option<Transaction>> {
        //todo: only expose canonized!!!
        let res = self
            .repository_manager
            .transaction_receipt_repository
            .get_by_hash(&h256_to_bytes32(hash));
        tracing::info!("get_transaction_by_hash: hash: {:?}, res: {:?}", hash, res);
        Ok(res.map(|data| data.transaction))
    }

    async fn get_transaction_by_block_hash_and_index(
        &self,
        _block_hash: H256,
        _index: Index,
    ) -> RpcResult<Option<Transaction>> {
        unimplemented!()
    }

    async fn get_transaction_by_block_number_and_index(
        &self,
        _block_number: BlockNumber,
        _index: Index,
    ) -> RpcResult<Option<Transaction>> {
        unimplemented!()
    }

    async fn get_transaction_receipt(&self, hash: H256) -> RpcResult<Option<TransactionReceipt>> {
        //todo: only expose canonized!!!
        let res = self
            .repository_manager
            .transaction_receipt_repository
            .get_by_hash(&h256_to_bytes32(hash));
        tracing::debug!("get_transaction_by_hash: hash: {:?}, res: {:?}", hash, res);
        Ok(res.map(|data| data.receipt))
    }

    async fn protocol_version(&self) -> RpcResult<String> {
        unimplemented!()
    }

    async fn send_raw_transaction(&self, tx_bytes: Bytes) -> RpcResult<H256> {
        let latency = API_METRICS.response_time[&"send_raw_transaction"].start();

        let r = self
            .tx_handler
            .send_raw_transaction_impl(tx_bytes)
            .map_err(|err| self.map_err(err));
        latency.observe();

        r
    }

    async fn syncing(&self) -> RpcResult<SyncState> {
        unimplemented!()
    }

    async fn accounts(&self) -> RpcResult<Vec<Address>> {
        unimplemented!()
    }

    async fn coinbase(&self) -> RpcResult<Address> {
        unimplemented!()
    }

    async fn compilers(&self) -> RpcResult<Vec<String>> {
        unimplemented!()
    }

    async fn hashrate(&self) -> RpcResult<U256> {
        unimplemented!()
    }

    async fn get_uncle_count_by_block_hash(&self, _hash: H256) -> RpcResult<Option<U256>> {
        unimplemented!()
    }

    async fn get_uncle_count_by_block_number(
        &self,
        _number: BlockNumber,
    ) -> RpcResult<Option<U256>> {
        unimplemented!()
    }

    async fn mining(&self) -> RpcResult<bool> {
        unimplemented!()
    }

    async fn fee_history(
        &self,
        _block_count: U64Number,
        _newest_block: BlockNumber,
        _reward_percentiles: Option<Vec<f32>>,
    ) -> RpcResult<FeeHistory> {
        unimplemented!()
    }

    async fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
        unimplemented!()
    }
}
