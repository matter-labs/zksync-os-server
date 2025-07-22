use alloy::consensus::Account;
use alloy::eips::eip1559::Eip1559Estimation;
use alloy::eips::eip2930::AccessListResult;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{Ethereum, EthereumWallet, Network};
use alloy::primitives::{
    Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, TxHash, U64, U128, U256,
};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{
    EthCall, EthCallMany, EthGetBlock, FilterPollerBuilder, PendingTransaction,
    PendingTransactionBuilder, PendingTransactionConfig, PendingTransactionError, Provider,
    ProviderCall, RootProvider, RpcWithBlock, SendableTx, WalletProvider,
};
use alloy::rpc::client::{ClientRef, NoParams, WeakClient};
use alloy::rpc::types::erc4337::TransactionConditional;
use alloy::rpc::types::simulate::{SimulatePayload, SimulatedBlock};
use alloy::rpc::types::{
    AccountInfo, Bundle, EIP1186AccountProofResponse, EthCallResponse, FeeHistory, Filter,
    FilterChanges, Index, Log, SyncStatus,
};
use alloy::transports::TransportResult;
use serde_json::value::RawValue;
use std::borrow::Cow;

/// A version of `Provider<Ethereum> + WalletProvider<Ethereum, Wallet = EthereumWallet>` that is
/// object safe. Has a blanket implementation for the aforementioned constraints.
pub trait EthWalletProvider: Provider<Ethereum> + 'static {
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider>;

    /// Get a reference to the underlying wallet.
    fn wallet(&self) -> &EthereumWallet;

    /// Get a mutable reference to the underlying wallet.
    fn wallet_mut(&mut self) -> &mut EthereumWallet;
}

impl<T> EthWalletProvider for T
where
    T: Provider<Ethereum> + WalletProvider<Ethereum, Wallet = EthereumWallet> + Clone + 'static,
{
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider> {
        Box::new(self.clone())
    }

    fn wallet(&self) -> &EthereumWallet {
        <Self as WalletProvider<Ethereum>>::wallet(self)
    }

    fn wallet_mut(&mut self) -> &mut EthereumWallet {
        <Self as WalletProvider<Ethereum>>::wallet_mut(self)
    }
}

/// A version of `DynProvider` that exposes `wallet()` and `wallet_mut()` as defined in
/// `EthWalletProvider`. Also uses `Box` instead of `Arc` to make sure the wallets are mutable.
pub struct EthDynProvider(Box<dyn EthWalletProvider + 'static>);

impl EthDynProvider {
    /// Creates a new [`EthDynProvider`] by erasing the type.
    pub fn new<P: EthWalletProvider + 'static>(provider: P) -> Self {
        Self(Box::new(provider))
    }
}

impl Clone for EthDynProvider {
    fn clone(&self) -> Self {
        EthDynProvider(self.dyn_clone())
    }
}

impl std::fmt::Debug for EthDynProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DynProvider")
            .field(&"<dyn Provider>")
            .finish()
    }
}

//
// The rest of the file contains trait implementations for `EthDynProvider` that just invoke `self.0.<method>` inside
//

#[async_trait::async_trait]
impl Provider<Ethereum> for EthDynProvider {
    fn root(&self) -> &RootProvider<Ethereum> {
        self.0.root()
    }

    fn client(&self) -> ClientRef<'_> {
        self.0.client()
    }

    fn weak_client(&self) -> WeakClient {
        self.0.weak_client()
    }

    fn get_accounts(&self) -> ProviderCall<NoParams, Vec<Address>> {
        self.0.get_accounts()
    }

    fn get_blob_base_fee(&self) -> ProviderCall<NoParams, U128, u128> {
        self.0.get_blob_base_fee()
    }

    fn get_block_number(&self) -> ProviderCall<NoParams, U64, BlockNumber> {
        self.0.get_block_number()
    }

    fn call(&self, tx: <Ethereum as Network>::TransactionRequest) -> EthCall<Ethereum, Bytes> {
        self.0.call(tx)
    }

    fn call_many<'req>(
        &self,
        bundles: &'req [Bundle],
    ) -> EthCallMany<'req, Ethereum, Vec<Vec<EthCallResponse>>> {
        self.0.call_many(bundles)
    }

    fn simulate<'req>(
        &self,
        payload: &'req SimulatePayload,
    ) -> RpcWithBlock<
        &'req SimulatePayload,
        Vec<SimulatedBlock<<Ethereum as Network>::BlockResponse>>,
    > {
        self.0.simulate(payload)
    }

    fn get_chain_id(&self) -> ProviderCall<NoParams, U64, u64> {
        self.0.get_chain_id()
    }

    fn create_access_list<'a>(
        &self,
        request: &'a <Ethereum as Network>::TransactionRequest,
    ) -> RpcWithBlock<&'a <Ethereum as Network>::TransactionRequest, AccessListResult> {
        self.0.create_access_list(request)
    }

    fn estimate_gas(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> EthCall<Ethereum, U64, u64> {
        self.0.estimate_gas(tx)
    }

    async fn estimate_eip1559_fees_with(
        &self,
        estimator: Eip1559Estimator,
    ) -> TransportResult<Eip1559Estimation> {
        self.0.estimate_eip1559_fees_with(estimator).await
    }

    async fn estimate_eip1559_fees(&self) -> TransportResult<Eip1559Estimation> {
        self.0.estimate_eip1559_fees().await
    }

    async fn get_fee_history(
        &self,
        block_count: u64,
        last_block: BlockNumberOrTag,
        reward_percentiles: &[f64],
    ) -> TransportResult<FeeHistory> {
        self.0
            .get_fee_history(block_count, last_block, reward_percentiles)
            .await
    }

    fn get_gas_price(&self) -> ProviderCall<NoParams, U128, u128> {
        self.0.get_gas_price()
    }

    fn get_account_info(&self, address: Address) -> RpcWithBlock<Address, AccountInfo> {
        self.0.get_account_info(address)
    }

    fn get_account(&self, address: Address) -> RpcWithBlock<Address, Account> {
        self.0.get_account(address)
    }

    fn get_balance(&self, address: Address) -> RpcWithBlock<Address, U256, U256> {
        self.0.get_balance(address)
    }

    fn get_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.0.get_block(block)
    }

    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.0.get_block_by_hash(hash)
    }

    fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.0.get_block_by_number(number)
    }

    async fn get_block_transaction_count_by_hash(
        &self,
        hash: BlockHash,
    ) -> TransportResult<Option<u64>> {
        self.0.get_block_transaction_count_by_hash(hash).await
    }

    async fn get_block_transaction_count_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> TransportResult<Option<u64>> {
        self.0
            .get_block_transaction_count_by_number(block_number)
            .await
    }

    fn get_block_receipts(
        &self,
        block: BlockId,
    ) -> ProviderCall<(BlockId,), Option<Vec<<Ethereum as Network>::ReceiptResponse>>> {
        self.0.get_block_receipts(block)
    }

    fn get_code_at(&self, address: Address) -> RpcWithBlock<Address, Bytes> {
        self.0.get_code_at(address)
    }

    async fn watch_blocks(&self) -> TransportResult<FilterPollerBuilder<B256>> {
        self.0.watch_blocks().await
    }

    async fn watch_pending_transactions(&self) -> TransportResult<FilterPollerBuilder<B256>> {
        self.0.watch_pending_transactions().await
    }

    async fn watch_logs(&self, filter: &Filter) -> TransportResult<FilterPollerBuilder<Log>> {
        self.0.watch_logs(filter).await
    }

    async fn watch_full_pending_transactions(
        &self,
    ) -> TransportResult<FilterPollerBuilder<<Ethereum as Network>::TransactionResponse>> {
        self.0.watch_full_pending_transactions().await
    }

    async fn get_filter_changes_dyn(&self, id: U256) -> TransportResult<FilterChanges> {
        self.0.get_filter_changes_dyn(id).await
    }

    async fn get_filter_logs(&self, id: U256) -> TransportResult<Vec<Log>> {
        self.0.get_filter_logs(id).await
    }

    async fn uninstall_filter(&self, id: U256) -> TransportResult<bool> {
        self.0.uninstall_filter(id).await
    }

    async fn watch_pending_transaction(
        &self,
        config: PendingTransactionConfig,
    ) -> Result<PendingTransaction, PendingTransactionError> {
        self.0.watch_pending_transaction(config).await
    }

    async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>> {
        self.0.get_logs(filter).await
    }

    fn get_proof(
        &self,
        address: Address,
        keys: Vec<StorageKey>,
    ) -> RpcWithBlock<(Address, Vec<StorageKey>), EIP1186AccountProofResponse> {
        self.0.get_proof(address, keys)
    }

    fn get_storage_at(
        &self,
        address: Address,
        key: U256,
    ) -> RpcWithBlock<(Address, U256), StorageValue> {
        self.0.get_storage_at(address, key)
    }

    fn get_transaction_by_hash(
        &self,
        hash: TxHash,
    ) -> ProviderCall<(TxHash,), Option<<Ethereum as Network>::TransactionResponse>> {
        self.0.get_transaction_by_hash(hash)
    }

    fn get_transaction_by_sender_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> ProviderCall<(Address, U64), Option<<Ethereum as Network>::TransactionResponse>> {
        self.0.get_transaction_by_sender_nonce(sender, nonce)
    }

    fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: usize,
    ) -> ProviderCall<(B256, Index), Option<<Ethereum as Network>::TransactionResponse>> {
        self.0
            .get_transaction_by_block_hash_and_index(block_hash, index)
    }

    fn get_raw_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: usize,
    ) -> ProviderCall<(B256, Index), Option<Bytes>> {
        self.0
            .get_raw_transaction_by_block_hash_and_index(block_hash, index)
    }

    fn get_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: usize,
    ) -> ProviderCall<(BlockNumberOrTag, Index), Option<<Ethereum as Network>::TransactionResponse>>
    {
        self.0
            .get_transaction_by_block_number_and_index(block_number, index)
    }

    fn get_raw_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: usize,
    ) -> ProviderCall<(BlockNumberOrTag, Index), Option<Bytes>> {
        self.0
            .get_raw_transaction_by_block_number_and_index(block_number, index)
    }

    fn get_raw_transaction_by_hash(&self, hash: TxHash) -> ProviderCall<(TxHash,), Option<Bytes>> {
        self.0.get_raw_transaction_by_hash(hash)
    }

    fn get_transaction_count(
        &self,
        address: Address,
    ) -> RpcWithBlock<Address, U64, u64, fn(U64) -> u64> {
        self.0.get_transaction_count(address)
    }

    fn get_transaction_receipt(
        &self,
        hash: TxHash,
    ) -> ProviderCall<(TxHash,), Option<<Ethereum as Network>::ReceiptResponse>> {
        self.0.get_transaction_receipt(hash)
    }

    async fn get_uncle(
        &self,
        tag: BlockId,
        idx: u64,
    ) -> TransportResult<Option<<Ethereum as Network>::BlockResponse>> {
        self.0.get_uncle(tag, idx).await
    }

    async fn get_uncle_count(&self, tag: BlockId) -> TransportResult<u64> {
        self.0.get_uncle_count(tag).await
    }

    fn get_max_priority_fee_per_gas(&self) -> ProviderCall<NoParams, U128, u128> {
        self.0.get_max_priority_fee_per_gas()
    }

    async fn new_block_filter(&self) -> TransportResult<U256> {
        self.0.new_block_filter().await
    }

    async fn new_filter(&self, filter: &Filter) -> TransportResult<U256> {
        self.0.new_filter(filter).await
    }

    async fn new_pending_transactions_filter(&self, full: bool) -> TransportResult<U256> {
        self.0.new_pending_transactions_filter(full).await
    }

    async fn send_raw_transaction(
        &self,
        encoded_tx: &[u8],
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.0.send_raw_transaction(encoded_tx).await
    }

    async fn send_raw_transaction_conditional(
        &self,
        encoded_tx: &[u8],
        conditional: TransactionConditional,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.0
            .send_raw_transaction_conditional(encoded_tx, conditional)
            .await
    }

    async fn send_transaction(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.0.send_transaction(tx).await
    }

    async fn send_tx_envelope(
        &self,
        tx: <Ethereum as Network>::TxEnvelope,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.0.send_tx_envelope(tx).await
    }

    async fn send_transaction_internal(
        &self,
        tx: SendableTx<Ethereum>,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.0.send_transaction_internal(tx).await
    }

    async fn sign_transaction(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> TransportResult<Bytes> {
        self.0.sign_transaction(tx).await
    }

    fn syncing(&self) -> ProviderCall<NoParams, SyncStatus> {
        self.0.syncing()
    }

    fn get_client_version(&self) -> ProviderCall<NoParams, String> {
        self.0.get_client_version()
    }

    fn get_sha3(&self, data: &[u8]) -> ProviderCall<(String,), B256> {
        self.0.get_sha3(data)
    }

    fn get_net_version(&self) -> ProviderCall<NoParams, U64, u64> {
        self.0.get_net_version()
    }

    async fn raw_request_dyn(
        &self,
        method: Cow<'static, str>,
        params: &RawValue,
    ) -> TransportResult<Box<RawValue>> {
        self.0.raw_request_dyn(method, params).await
    }

    fn transaction_request(&self) -> <Ethereum as Network>::TransactionRequest {
        self.0.transaction_request()
    }
}

impl EthWalletProvider for EthDynProvider {
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider> {
        self.0.dyn_clone()
    }

    fn wallet(&self) -> &EthereumWallet {
        self.0.wallet()
    }

    fn wallet_mut(&mut self) -> &mut EthereumWallet {
        self.0.wallet_mut()
    }
}
