use crate::eth_call_handler::EthCallHandler;
use crate::result::{ToRpcResult, internal_rpc_err, unimplemented_rpc_err};
use crate::rpc_storage::{ReadRpcStorage, RpcStorageError};
use crate::tx_handler::TxHandler;
use alloy::consensus::Account;
use alloy::consensus::transaction::Recovered;
use alloy::dyn_abi::TypedData;
use alloy::eips::eip2930::AccessListResult;
use alloy::eips::{BlockId, BlockNumberOrTag, Encodable2718};
use alloy::network::BlockResponse;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, B256, Bytes, TxHash, U64, U256};
use alloy::rpc::types::simulate::{SimulatePayload, SimulatedBlock};
use alloy::rpc::types::state::StateOverride;
use alloy::rpc::types::{
    AccountInfo, BlockOverrides, Bundle, EIP1186AccountProofResponse, EthCallResponse, FeeHistory,
    Index, Log, StateContext, SyncStatus, TransactionRequest,
};
use alloy::serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use std::convert::identity;
use tokio::sync::watch;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_api::helpers::{get_balance, get_code};
use zksync_os_interface::traits::ReadStorage;
use zksync_os_mempool::L2TransactionPool;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_rpc_api::types::{
    RpcBlockConvert, ZkApiBlock, ZkApiTransaction, ZkHeader, ZkTransactionReceipt,
};
use zksync_os_storage_api::{RepositoryError, StateError, TxMeta, ViewState};
use zksync_os_types::{L2Envelope, TransactionAcceptanceState, ZkReceiptEnvelope};

pub struct EthNamespace<RpcStorage, Mempool> {
    tx_handler: TxHandler<Mempool>,
    eth_call_handler: EthCallHandler<RpcStorage>,

    // todo: the idea is to only have handlers here, but then get_balance would require its own handler
    // reconsider approach to API in this regard
    storage: RpcStorage,
    mempool: Mempool,

    chain_id: u64,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> EthNamespace<RpcStorage, Mempool> {
    pub fn new(
        storage: RpcStorage,
        mempool: Mempool,
        eth_call_handler: EthCallHandler<RpcStorage>,
        chain_id: u64,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    ) -> Self {
        let tx_handler = TxHandler::new(mempool.clone(), acceptance_state);

        Self {
            tx_handler,
            eth_call_handler,
            storage,
            mempool,
            chain_id,
        }
    }
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> EthNamespace<RpcStorage, Mempool> {
    fn block_number_impl(&self) -> EthResult<U256> {
        Ok(U256::from(self.storage.repository().get_latest_block()))
    }

    fn block_by_id_impl(
        &self,
        block_id: Option<BlockId>,
        // `full=false` means that the returned block will contain a list of transaction hashes.
        // `full=true` means that the returned block will contain a list of transactions (in RPC representation).
        full: bool,
    ) -> EthResult<Option<ZkApiBlock>> {
        let block_id = block_id.unwrap_or_default();
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let mut rpc_block = block.into_rpc();
        if full {
            let tx_hashes = rpc_block.transactions().hashes();
            let mut full_txs = Vec::with_capacity(tx_hashes.len());
            for tx_hash in tx_hashes {
                if let Some(tx) = self.storage.repository().get_transaction(tx_hash)?
                    && let Some(meta) = self.storage.repository().get_transaction_meta(tx_hash)?
                {
                    full_txs.push(build_api_tx(tx, Some(&meta)));
                } else {
                    return Err(EthError::BlockNotFound(block_id));
                }
            }
            rpc_block.transactions = BlockTransactions::Full(full_txs);
        }
        Ok(Some(rpc_block))
    }

    fn block_transaction_count_by_id_impl(&self, block_id: BlockId) -> EthResult<Option<U256>> {
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        Ok(Some(U256::from(block.body.transactions.len())))
    }

    fn block_receipts_impl(
        &self,
        block_id: BlockId,
    ) -> EthResult<Option<Vec<ZkTransactionReceipt>>> {
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
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
        let block = self.storage.get_block_by_id(block_id)?;
        if block.is_some() {
            // ZKsync OS is not PoW and hence does not have uncle blocks
            Ok(Some(U256::ZERO))
        } else {
            Ok(None)
        }
    }

    fn raw_transaction_by_hash_impl(&self, hash: B256) -> EthResult<Option<Bytes>> {
        // Look up in mempool first to avoid race condition
        if let Some(pool_tx) = self.mempool.get(&hash) {
            return Ok(Some(Bytes::from(
                pool_tx.transaction.transaction.encoded_2718(),
            )));
        }
        if let Some(raw_tx) = self.storage.repository().get_raw_transaction(hash)? {
            return Ok(Some(Bytes::from(raw_tx)));
        }
        Ok(None)
    }

    fn transaction_by_hash_impl(&self, hash: B256) -> EthResult<Option<ZkApiTransaction>> {
        // Look up in mempool first to avoid race condition
        if let Some(pool_tx) = self.mempool.get(&hash) {
            let envelope = L2Envelope::from(pool_tx.transaction.transaction.inner().clone());
            return Ok(Some(build_api_tx(
                Recovered::new_unchecked(envelope, pool_tx.transaction.transaction.signer()).into(),
                None,
            )));
        }
        if let Some(tx) = self.storage.repository().get_transaction(hash)?
            && let Some(meta) = self.storage.repository().get_transaction_meta(hash)?
        {
            return Ok(Some(build_api_tx(tx, Some(&meta))));
        }
        Ok(None)
    }

    fn raw_transaction_by_block_id_and_index_impl(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> EthResult<Option<Bytes>> {
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let Some(tx_hash) = block.body.transactions.get(index.0) else {
            return Ok(None);
        };
        Ok(self
            .storage
            .repository()
            .get_raw_transaction(*tx_hash)?
            .map(Bytes::from))
    }

    fn transaction_by_block_id_and_index_impl(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> EthResult<Option<ZkApiTransaction>> {
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
            return Ok(None);
        };
        let Some(tx_hash) = block.body.transactions.get(index.0) else {
            return Ok(None);
        };
        let Some(tx) = self.storage.repository().get_transaction(*tx_hash)? else {
            return Ok(None);
        };
        let Some(meta) = self.storage.repository().get_transaction_meta(*tx_hash)? else {
            return Ok(None);
        };
        Ok(Some(build_api_tx(tx, Some(&meta))))
    }

    fn transaction_by_sender_and_nonce_impl(
        &self,
        sender: Address,
        nonce: U64,
    ) -> EthResult<Option<ZkApiTransaction>> {
        let Some(tx_hash) = self
            .storage
            .repository()
            .get_transaction_hash_by_sender_nonce(sender, nonce.saturating_to())?
        else {
            return Ok(None);
        };
        self.transaction_by_hash_impl(tx_hash)
    }

    fn transaction_receipt_impl(&self, tx_hash: B256) -> EthResult<Option<ZkTransactionReceipt>> {
        let Some(stored_tx) = self.storage.repository().get_stored_transaction(tx_hash)? else {
            return Ok(None);
        };
        Ok(Some(build_api_receipt(
            tx_hash,
            stored_tx.receipt,
            &stored_tx.tx,
            &stored_tx.meta,
        )))
    }

    fn balance_impl(&self, address: Address, block_id: Option<BlockId>) -> EthResult<U256> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };
        Ok(self
            .storage
            .state_view_at(block_number)?
            .get_account(address)
            .as_ref()
            .map(get_balance)
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
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        let flat_key = derive_flat_storage_key(
            &B160::from_be_bytes(address.into_array()),
            &(key.as_b256().0.into()),
        );
        let mut state = self.storage.state_view_at(block_number)?;
        Ok(state
            .read(flat_key.as_u8_array().into())
            .unwrap_or_default())
    }

    fn transaction_count_impl(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> EthResult<U256> {
        let on_chain_account_nonce = self
            .storage
            .state_at_block_id_or_latest(block_id)?
            .account_nonce(address)
            .unwrap_or(0);

        if block_id == Some(BlockId::pending())
            && let Some(highest_pool_tx) = self
                .mempool
                .get_highest_consecutive_transaction_by_sender(address, on_chain_account_nonce)
        {
            // Pending block id has special meaning in `eth_getTransactionCount`: it takes pending
            // mempool transactions into account. We take highest tx in the pool and use its nonce + 1
            // (on chain nonce is increased after tx gets executed).
            let next_tx_nonce = highest_pool_tx
                .nonce()
                .checked_add(1)
                .ok_or(EthError::NonceMaxValue)?;

            Ok(U256::from(next_tx_nonce))
        } else {
            Ok(U256::from(on_chain_account_nonce))
        }
    }

    fn get_code_impl(&self, address: Address, block_id: Option<BlockId>) -> EthResult<Bytes> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default();
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        // todo(#36): distinguish between N/A blocks and actual missing accounts
        let mut view = self.storage.state_view_at(block_number)?;
        let Some(props) = view.get_account(address) else {
            return Ok(Bytes::default());
        };
        let bytecode = get_code(&mut view, &props);
        Ok(Bytes::copy_from_slice(&bytecode))
    }

    fn gas_price_impl(&self) -> EthResult<U256> {
        // Only base fee is taken into account, suggested priority fee is zero.
        if let Some(c) = self.eth_call_handler.pending_block_context() {
            Ok(c.eip1559_basefee)
        } else {
            let latest_block_id = BlockId::Number(BlockNumberOrTag::Latest);
            let Some(resolved_block_number) = self.storage.resolve_block_number(latest_block_id)?
            else {
                return Err(EthError::BlockNotFound(latest_block_id));
            };
            self.storage
                .replay_storage()
                .get_context(resolved_block_number)
                .map(|c| c.eip1559_basefee)
                .ok_or(EthError::BlockNotFound(latest_block_id))
        }
    }

    fn fee_history_impl(
        &self,
        block_count: U64,
        mut newest_block: BlockNumberOrTag,
        _reward_percentiles: Option<Vec<f64>>,
    ) -> EthResult<FeeHistory> {
        if block_count == 0 {
            return Ok(FeeHistory::default());
        }
        if newest_block.is_pending() {
            // cap the target block since we don't have fee history for the pending block
            newest_block = BlockNumberOrTag::Latest;
        }
        let Some(end_block) = self.storage.resolve_block_number(newest_block.into())? else {
            return Err(EthError::BlockNotFound(newest_block.into()));
        };

        let end_block_plus = end_block + 1;
        // Ensure that we would not be querying outside of genesis
        let block_count = end_block_plus.min(block_count.try_into().unwrap());
        let start_block = end_block_plus - block_count;

        let mut base_fee_per_gas = Vec::with_capacity(block_count as usize + 1);
        for block in start_block..=end_block {
            let base_fee = self
                .storage
                .replay_storage()
                .get_context(block)
                .map(|c| c.eip1559_basefee)
                .ok_or(EthError::BlockNotFound(BlockId::Number(
                    BlockNumberOrTag::Number(block),
                )))?;
            base_fee_per_gas.push(base_fee.saturating_to());
        }
        if let Some(base_fee) = self
            .storage
            .replay_storage()
            .get_context(end_block_plus)
            .map(|c| c.eip1559_basefee)
        {
            base_fee_per_gas.push(base_fee.saturating_to());
        } else if let Some(c) = self.eth_call_handler.pending_block_context()
            && c.block_number == end_block_plus
        {
            base_fee_per_gas.push(c.eip1559_basefee.saturating_to());
        } else {
            // block_count is >= 1 so last must be there.
            base_fee_per_gas.push(*base_fee_per_gas.last().unwrap());
        }

        Ok(FeeHistory {
            base_fee_per_gas,
            oldest_block: start_block,
            // Conventional values.
            gas_used_ratio: vec![0.5; block_count as usize],
            base_fee_per_blob_gas: vec![],
            blob_gas_used_ratio: vec![],
            // TODO: fill reward
            reward: None,
        })
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> EthApiServer
    for EthNamespace<RpcStorage, Mempool>
{
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

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<ZkApiBlock>> {
        self.block_by_id_impl(Some(hash.into()), full)
            .to_rpc_result()
    }

    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<ZkApiBlock>> {
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
    ) -> RpcResult<Option<Vec<ZkTransactionReceipt>>> {
        self.block_receipts_impl(block_id).to_rpc_result()
    }

    async fn uncle_by_block_hash_and_index(
        &self,
        _hash: B256,
        _index: Index,
    ) -> RpcResult<Option<ZkApiBlock>> {
        // ZKsync OS is not PoW and hence does not have uncle blocks
        Ok(None)
    }

    async fn uncle_by_block_number_and_index(
        &self,
        _number: BlockNumberOrTag,
        _index: Index,
    ) -> RpcResult<Option<ZkApiBlock>> {
        // ZKsync OS is not PoW and hence does not have uncle blocks
        Ok(None)
    }

    async fn raw_transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Bytes>> {
        self.raw_transaction_by_hash_impl(hash).to_rpc_result()
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<ZkApiTransaction>> {
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
    ) -> RpcResult<Option<ZkApiTransaction>> {
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
    ) -> RpcResult<Option<ZkApiTransaction>> {
        self.transaction_by_block_id_and_index_impl(number.into(), index)
            .to_rpc_result()
    }

    async fn transaction_by_sender_and_nonce(
        &self,
        address: Address,
        nonce: U64,
    ) -> RpcResult<Option<ZkApiTransaction>> {
        self.transaction_by_sender_and_nonce_impl(address, nonce)
            .to_rpc_result()
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<ZkTransactionReceipt>> {
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

    async fn header_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> RpcResult<Option<ZkHeader>> {
        Ok(self
            .block_by_id_impl(Some(block_number.into()), false)
            .to_rpc_result()?
            .map(|block| block.header))
    }

    async fn header_by_hash(&self, hash: B256) -> RpcResult<Option<ZkHeader>> {
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
        self.eth_call_handler
            .call_impl(request, block_number, state_overrides, block_overrides)
            .to_rpc_result()
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
        // todo(#119)
        Err(unimplemented_rpc_err())
    }

    async fn estimate_gas(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<U256> {
        self.eth_call_handler
            .estimate_gas_impl(request, block_number, state_override)
            .to_rpc_result()
    }

    async fn gas_price(&self) -> RpcResult<U256> {
        self.gas_price_impl().to_rpc_result()
    }

    async fn get_account(&self, _address: Address, _block: BlockId) -> RpcResult<Option<Account>> {
        // todo(#36): implement
        Err(unimplemented_rpc_err())
    }

    async fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
        Ok(U256::from(0))
    }

    async fn blob_base_fee(&self) -> RpcResult<U256> {
        // todo(EIP-4844)
        Err(unimplemented_rpc_err())
    }

    async fn fee_history(
        &self,
        block_count: U64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<FeeHistory> {
        self.fee_history_impl(block_count, newest_block, reward_percentiles)
            .to_rpc_result()
    }

    async fn send_transaction(&self, _request: TransactionRequest) -> RpcResult<B256> {
        Err(internal_rpc_err("node has no signer accounts"))
    }

    async fn send_raw_transaction(&self, bytes: Bytes) -> RpcResult<B256> {
        self.tx_handler
            .send_raw_transaction_impl(bytes)
            .await
            .to_rpc_result()
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

pub fn build_api_log(
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

pub fn build_api_receipt(
    tx_hash: TxHash,
    receipt: ZkReceiptEnvelope,
    tx: &zksync_os_types::ZkTransaction,
    meta: &TxMeta,
) -> ZkTransactionReceipt {
    let mut log_index_in_tx = 0;
    let inner = receipt.map_logs(
        |inner_log| {
            let log = build_api_log(tx_hash, inner_log, meta.clone(), log_index_in_tx);
            log_index_in_tx += 1;
            log
        },
        // todo: convert L2->L1 logs to RPC variant when we have one
        identity,
    );
    ZkTransactionReceipt {
        inner,
        transaction_hash: tx_hash,
        transaction_index: Some(meta.tx_index_in_block),
        block_hash: Some(meta.block_hash),
        block_number: Some(meta.block_number),
        gas_used: meta.gas_used,
        effective_gas_price: meta.effective_gas_price,
        blob_gas_used: None,
        blob_gas_price: None,
        from: tx.signer(),
        to: tx.to(),
        contract_address: meta.contract_address,
    }
}

pub fn build_api_tx(tx: zksync_os_types::ZkTransaction, meta: Option<&TxMeta>) -> ZkApiTransaction {
    ZkApiTransaction {
        inner: tx.inner,
        block_hash: meta.map(|meta| meta.block_hash),
        block_number: meta.map(|meta| meta.block_number),
        transaction_index: meta.map(|meta| meta.tx_index_in_block),
        effective_gas_price: meta.map(|meta| meta.effective_gas_price),
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
    /// Returned if the nonce of a transaction is too high
    /// Incrementing the nonce would lead to invalid state (overflow)
    #[error("nonce has max value")]
    NonceMaxValue,

    #[error(transparent)]
    RpcStorage(#[from] RpcStorageError),

    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    State(#[from] StateError),
}
