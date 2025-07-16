use crate::api::eth_call_handler::EthCallHandler;
use crate::api::metrics::API_METRICS;
use crate::api::resolve_block_id;
use crate::api::result::{internal_rpc_err, unimplemented_rpc_err, ToRpcResult};
use crate::api::tx_handler::TxHandler;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use crate::reth_state::ZkClient;
use crate::CHAIN_ID;
use alloy::consensus::Sealable;
use alloy::dyn_abi::TypedData;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, Bytes, B256, U256, U64};
use alloy::rpc::types::state::StateOverride;
use alloy::rpc::types::{
    Block, BlockOverrides, EIP1186AccountProofResponse, FeeHistory, Header, Index, SyncStatus,
    Transaction, TransactionReceipt, TransactionRequest,
};
use alloy::serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zk_ee::utils::Bytes32;
use zksync_os_mempool::RethPool;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_state::StateHandle;
use zksync_os_types::ZkEnvelope;

pub(crate) struct EthNamespace {
    tx_handler: TxHandler,
    eth_call_handler: EthCallHandler,

    // todo: the idea is to only have handlers here, but then get_balance would require its own handler
    // reconsider approach to API in this regard
    pub(super) repository_manager: RepositoryManager,

    pub(super) finality_info: FinalityTracker,
    pub(super) chain_id: u64,
}

impl EthNamespace {
    pub fn new(
        config: RpcConfig,

        repository_manager: RepositoryManager,
        finality_tracker: FinalityTracker,
        state_handle: StateHandle,
        mempool: RethPool<ZkClient>,
        block_replay_storage: BlockReplayStorage,
    ) -> Self {
        let tx_handler = TxHandler::new(mempool);

        let eth_call_handler = EthCallHandler::new(
            config,
            finality_tracker.clone(),
            state_handle,
            block_replay_storage,
            repository_manager.account_property_repository.clone(),
        );
        Self {
            tx_handler,
            eth_call_handler,
            repository_manager,
            finality_info: finality_tracker,
            chain_id: CHAIN_ID,
        }
    }
}

impl EthNamespace {
    fn block_by_id_impl(&self, id: Option<BlockId>, full: bool) -> RpcResult<Option<Block>> {
        // todo: to be re-implemented in https://github.com/matter-labs/zksync-os-server/issues/29
        if full {
            return Err(internal_rpc_err("full blocks are not supported yet"));
        }
        let id = id.unwrap_or(BlockId::Number(BlockNumberOrTag::Pending));
        let number = resolve_block_id(id, &self.finality_info);
        Ok(self
            .repository_manager
            .block_receipt_repository
            .get_by_number(number)
            .map(|(block_output, tx_hashes)| {
                let header = alloy::consensus::Header {
                    number: block_output.header.number,
                    timestamp: block_output.header.timestamp,
                    gas_limit: block_output.header.gas_limit,
                    base_fee_per_gas: Some(block_output.header.base_fee_per_gas),
                    ..Default::default()
                };
                Block::new(
                    Header::from_consensus(header.seal_slow(), None, None),
                    BlockTransactions::Hashes(tx_hashes),
                )
            }))
    }
}

#[async_trait]
impl EthApiServer for EthNamespace {
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
        Ok(U256::from(self.finality_info.get_canonized_block()))
    }

    async fn chain_id(&self) -> RpcResult<Option<U64>> {
        Ok(Some(U64::from(self.chain_id)))
    }

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<Block>> {
        // todo: to be re-implemented in https://github.com/matter-labs/zksync-os-server/issues/29
        self.block_by_id_impl(Some(hash.into()), full)
    }

    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<Block>> {
        // todo: to be re-implemented in https://github.com/matter-labs/zksync-os-server/issues/29
        self.block_by_id_impl(Some(number.into()), full)
    }

    async fn block_transaction_count_by_hash(&self, _hash: B256) -> RpcResult<Option<U256>> {
        todo!()
    }

    async fn block_transaction_count_by_number(
        &self,
        _number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        todo!()
    }

    async fn block_uncles_count_by_hash(&self, _hash: B256) -> RpcResult<Option<U256>> {
        todo!()
    }

    async fn block_uncles_count_by_number(
        &self,
        _number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        todo!()
    }

    async fn block_receipts(
        &self,
        _block_id: BlockId,
    ) -> RpcResult<Option<Vec<TransactionReceipt>>> {
        todo!()
    }

    async fn uncle_by_block_hash_and_index(
        &self,
        _hash: B256,
        _index: Index,
    ) -> RpcResult<Option<Block>> {
        todo!()
    }

    async fn uncle_by_block_number_and_index(
        &self,
        _number: BlockNumberOrTag,
        _index: Index,
    ) -> RpcResult<Option<Block>> {
        todo!()
    }

    async fn raw_transaction_by_hash(&self, _hash: B256) -> RpcResult<Option<Bytes>> {
        todo!()
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction<ZkEnvelope>>> {
        //todo: only expose canonized!!!
        let res = self
            .repository_manager
            .transaction_receipt_repository
            .get_by_hash(&Bytes32::from(hash.0));
        tracing::info!("get_transaction_by_hash: hash: {:?}, res: {:?}", hash, res);
        Ok(res.map(|data| data.transaction))
    }

    async fn raw_transaction_by_block_hash_and_index(
        &self,
        _hash: B256,
        _index: Index,
    ) -> RpcResult<Option<Bytes>> {
        todo!()
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        _hash: B256,
        _index: Index,
    ) -> RpcResult<Option<Transaction>> {
        todo!()
    }

    async fn raw_transaction_by_block_number_and_index(
        &self,
        _number: BlockNumberOrTag,
        _index: Index,
    ) -> RpcResult<Option<Bytes>> {
        todo!()
    }

    async fn transaction_by_block_number_and_index(
        &self,
        _number: BlockNumberOrTag,
        _index: Index,
    ) -> RpcResult<Option<Transaction>> {
        todo!()
    }

    async fn transaction_by_sender_and_nonce(
        &self,
        _address: Address,
        _nonce: U64,
    ) -> RpcResult<Option<Transaction>> {
        todo!()
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<TransactionReceipt>> {
        //todo: only expose canonized!!!
        let res = self
            .repository_manager
            .transaction_receipt_repository
            .get_by_hash(&Bytes32::from(hash.0));
        tracing::debug!("transaction_receipt: hash: {:?}, res: {:?}", hash, res);
        Ok(res.map(|data| data.receipt))
    }

    async fn balance(&self, address: Address, block_number: Option<BlockId>) -> RpcResult<U256> {
        //todo Daniyar: really add +1?
        let block_number = resolve_block_id(
            block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Pending)),
            &self.finality_info,
        ) + 1;
        let balance = self
            .repository_manager
            .account_property_repository
            .get_at_block(block_number, &address)
            .map(|props| props.balance)
            .unwrap_or(U256::ZERO);

        // tracing::info!(
        //     "get_balance: address: {:?}, block: {:?}, balance: {:?}",
        //     address,
        //     block,
        //     balance
        // );

        Ok(balance)
    }

    async fn storage_at(
        &self,
        _address: Address,
        _index: JsonStorageKey,
        _block_number: Option<BlockId>,
    ) -> RpcResult<B256> {
        todo!()
    }

    async fn transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        // returning nonce from ethemeral txs
        let nonce = self
            .repository_manager
            .account_property_repository
            .get_latest(&address)
            .map(|props| props.nonce)
            .unwrap_or(0);

        let resolved_block = resolve_block_id(
            block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Pending)),
            &self.finality_info,
        ) + 1;

        tracing::info!(
            ?address,
            ?block_number,
            resolved_block,
            nonce,
            "get_transaction_count resolved",
        );

        Ok(U256::from(nonce))
    }

    async fn get_code(&self, address: Address, block_number: Option<BlockId>) -> RpcResult<Bytes> {
        let resolved_block = resolve_block_id(
            block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Pending)),
            &self.finality_info,
        );

        let Some(props) = self
            .repository_manager
            .account_property_repository
            .get_at_block(resolved_block, &address)
        else {
            return Ok(Bytes::default());
        };
        Ok(Bytes::from(
            self.repository_manager
                .bytecode_repository
                .get_at_block(
                    resolved_block,
                    &B256::from(props.bytecode_hash.as_u8_array()),
                )
                .unwrap_or_default(),
        ))
    }

    async fn header_by_number(&self, _hash: BlockNumberOrTag) -> RpcResult<Option<Header>> {
        todo!()
    }

    async fn header_by_hash(&self, _hash: B256) -> RpcResult<Option<Header>> {
        todo!()
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

    async fn estimate_gas(
        &self,
        _request: TransactionRequest,
        _block_number: Option<BlockId>,
        _state_override: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let latency = API_METRICS.response_time[&"estimate_gas"].start();
        latency.observe();
        Ok(U256::from(1000000))
    }

    async fn gas_price(&self) -> RpcResult<U256> {
        Ok(U256::from(1000))
    }

    async fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
        todo!()
    }

    async fn blob_base_fee(&self) -> RpcResult<U256> {
        todo!()
    }

    async fn fee_history(
        &self,
        block_count: U64,
        _newest_block: BlockNumberOrTag,
        _reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<FeeHistory> {
        // todo: real implementation
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

    async fn is_mining(&self) -> RpcResult<bool> {
        todo!()
    }

    async fn hashrate(&self) -> RpcResult<U256> {
        todo!()
    }

    async fn send_transaction(&self, _request: TransactionRequest) -> RpcResult<B256> {
        todo!()
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
        todo!()
    }

    async fn sign_transaction(&self, _transaction: TransactionRequest) -> RpcResult<Bytes> {
        todo!()
    }

    async fn sign_typed_data(&self, _address: Address, _data: TypedData) -> RpcResult<Bytes> {
        todo!()
    }

    async fn get_proof(
        &self,
        _address: Address,
        _keys: Vec<JsonStorageKey>,
        _block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        todo!()
    }
}
