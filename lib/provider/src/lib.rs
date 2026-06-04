//! The node's canonical Ethereum-network provider.
//!
//! [`NodeProvider`] is an object-safe, wallet-capable wrapper over
//! [`alloy::providers::Provider<N>`] (defaulting to `Ethereum`) used everywhere the node talks to
//! an L1, Gateway, or L2 RPC. On top of the plain provider it caches per-address contract deployment blocks (see
//! [`NodeProvider::deployment_block`]), so the many startup binary searches over L1 history can use
//! a tight lower bound without each rediscovering it.

pub mod contracts;
pub mod l1_discovery;
mod metrics;
pub mod network;
pub mod settlement_layer_intervals;

pub use contracts::{Bridgehub, MessageRoot, MultisigCommitter, ZkChain, is_method_missing};

use crate::network::{SettlementLayer, Zksync};
use alloy::consensus::{BlockHeader, TrieAccount};
use alloy::eips::eip1559::Eip1559Estimation;
use alloy::eips::eip2930::AccessListResult;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::BlockResponse;
use alloy::network::{Ethereum, EthereumWallet, Network, NetworkWallet};
use alloy::primitives::{
    Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, TxHash, U64, U128, U256,
};
use alloy::providers::fillers::{RecommendedFillers, TxFiller};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{
    EthCall, EthCallMany, EthGetBlock, FilterPollerBuilder, PendingTransaction,
    PendingTransactionBuilder, PendingTransactionConfig, PendingTransactionError, Provider,
    ProviderBuilder, ProviderCall, RootProvider, RpcWithBlock, SendableTx, WalletProvider,
};
use alloy::rpc::client::{ClientRef, NoParams, RpcClient, WeakClient};
use alloy::rpc::types::erc4337::TransactionConditional;
use alloy::rpc::types::simulate::{SimulatePayload, SimulatedBlock};
use alloy::rpc::types::{
    AccountInfo, Bundle, EIP1186AccountProofResponse, EthCallResponse, FeeHistory, Filter,
    FilterChanges, Index, Log, SyncStatus,
};
use alloy::transports::TransportResult;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// A version of `Provider<N> + WalletProvider<N, Wallet = EthereumWallet>` that is
/// object safe. Has a blanket implementation for the aforementioned constraints.
pub trait EthWalletProvider<N: Network>: Provider<N> + 'static {
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider<N>>;

    /// Get a reference to the underlying wallet.
    fn wallet(&self) -> &EthereumWallet;

    /// Get a mutable reference to the underlying wallet.
    fn wallet_mut(&mut self) -> &mut EthereumWallet;
}

impl<N: Network, T> EthWalletProvider<N> for T
where
    T: Provider<N> + WalletProvider<N, Wallet = EthereumWallet> + Clone + 'static,
{
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider<N>> {
        Box::new(self.clone())
    }

    fn wallet(&self) -> &EthereumWallet {
        <Self as WalletProvider<N>>::wallet(self)
    }

    fn wallet_mut(&mut self) -> &mut EthereumWallet {
        <Self as WalletProvider<N>>::wallet_mut(self)
    }
}

/// Per-address cache of contract deployment blocks. Cloning a [`NodeProvider`] shares this cache
/// (it sits behind an `Arc`), so all derived contract instances and watchers resolve each address
/// at most once. Each address gets its own [`OnceCell`] so concurrent lookups for the same address
/// run the binary search exactly once and the rest await its result.
type DeploymentBlockCache = Arc<Mutex<HashMap<Address, Arc<OnceCell<u64>>>>>;

/// A version of `DynProvider` that exposes `wallet()` and `wallet_mut()` as defined in
/// `EthWalletProvider`. Also uses `Box` instead of `Arc` to make sure the wallets are mutable.
///
/// Carries a shared [`DeploymentBlockCache`]; see [`NodeProvider::deployment_block`].
pub struct NodeProvider<N: Network = Ethereum> {
    inner: Box<dyn EthWalletProvider<N> + 'static>,
    deployment_blocks: DeploymentBlockCache,
    /// [`Zksync`]-typed view of the same connection. Only ever `Some` for
    /// [`SettlementLayer`]-typed providers created via [`NodeProvider::from_gateway`]; see
    /// [`NodeProvider::zksync`].
    zksync_view: Option<Box<NodeProvider<Zksync>>>,
}

impl<N: Network> NodeProvider<N> {
    /// Creates a new [`NodeProvider`] by erasing the type.
    pub fn new<P: EthWalletProvider<N> + 'static>(provider: P) -> Self {
        Self {
            inner: Box::new(provider),
            deployment_blocks: Arc::new(Mutex::new(HashMap::new())),
            zksync_view: None,
        }
    }

    /// Re-views this connection under another network `M`: the rebuilt provider shares the
    /// underlying transport stack (including any retry/latency layers baked into it) and gets a
    /// fresh recommended fill stack plus a clone of the current wallet.
    ///
    /// Note: pubsub frontends are not carried over — node connections are HTTP + polling, which
    /// is all the rebuilt views need.
    fn reconnect_as<M>(&self) -> impl EthWalletProvider<M> + Clone
    where
        M: Network + RecommendedFillers,
        M::RecommendedFillers: TxFiller<M>,
        EthereumWallet: NetworkWallet<M>,
    {
        let inner = self.client();
        let client = RpcClient::new(inner.transport().clone(), false)
            .with_poll_interval(inner.poll_interval());
        ProviderBuilder::new_with_network::<M>()
            .wallet(self.inner.wallet().clone())
            .connect_client(client)
    }

    /// Returns the block at which `address` first had non-empty code, i.e. its deployment block.
    /// Returns `0` if `address` has no code at the latest block (not deployed on this chain), which
    /// keeps it usable as a binary-search lower bound on chains where the contract is absent (e.g.
    /// local Anvil setups).
    ///
    /// The result is cached per address and shared across clones; the underlying binary search over
    /// `eth_getCode` runs at most once per address for the lifetime of the cache.
    pub async fn deployment_block(&self, address: Address) -> anyhow::Result<u64> {
        let cell = {
            let mut guard = self
                .deployment_blocks
                .lock()
                .expect("deployment block cache mutex poisoned");
            guard.entry(address).or_default().clone()
        };
        let block = cell
            .get_or_try_init(|| Self::discover_deployment_block(self, address))
            .await?;
        Ok(*block)
    }

    /// Binary-searches for the first block where `address` has non-empty code. See
    /// [`Self::deployment_block`] for the `0` fallback semantics.
    async fn discover_deployment_block(&self, address: Address) -> anyhow::Result<u64> {
        let latest = self.get_block_number().await?;
        let code_at_latest = self.get_code_at(address).block_id(latest.into()).await?;
        if code_at_latest.0.is_empty() {
            return Ok(0);
        }
        let (mut lo, mut hi) = (0u64, latest);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let code = self.get_code_at(address).block_id(mid.into()).await?;
            if !code.0.is_empty() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        tracing::debug!(%address, deployment_block = lo, "discovered contract deployment block");
        Ok(lo)
    }
}

impl NodeProvider<SettlementLayer> {
    /// Views an existing L1 connection as a settlement layer. The new provider shares the L1
    /// connection's transport and deployment-block cache; [`Self::zksync`] returns `None`.
    pub fn from_l1(l1: &NodeProvider<Ethereum>) -> Self {
        Self {
            inner: Box::new(l1.reconnect_as::<SettlementLayer>()),
            deployment_blocks: l1.deployment_blocks.clone(),
            zksync_view: None,
        }
    }

    /// Views an existing Gateway connection as a settlement layer. The new provider shares the
    /// Gateway connection's transport and deployment-block cache, and retains the [`Zksync`]-typed
    /// view so it can be recovered via [`Self::zksync`].
    pub fn from_gateway(gateway: &NodeProvider<Zksync>) -> Self {
        Self {
            inner: Box::new(gateway.reconnect_as::<SettlementLayer>()),
            deployment_blocks: gateway.deployment_blocks.clone(),
            zksync_view: Some(Box::new(gateway.clone())),
        }
    }

    /// Whether this settlement layer is a ZKsync Gateway chain (as opposed to Ethereum L1).
    pub fn is_gateway(&self) -> bool {
        self.zksync_view.is_some()
    }

    /// [`Zksync`]-typed view of the same connection, available iff the settlement layer is a
    /// Gateway (i.e. this provider was created via [`Self::from_gateway`]).
    ///
    /// The returned provider's wallet is synced with this provider's *current* wallet, so signers
    /// registered after construction carry over to the ZK-typed view.
    pub fn zksync(&self) -> Option<NodeProvider<Zksync>> {
        self.zksync_view.as_deref().map(|view| {
            let mut view = view.clone();
            *view.inner.wallet_mut() = self.inner.wallet().clone();
            view
        })
    }
}

impl<N: Network> Clone for NodeProvider<N> {
    fn clone(&self) -> Self {
        NodeProvider {
            inner: self.inner.dyn_clone(),
            deployment_blocks: self.deployment_blocks.clone(),
            zksync_view: self.zksync_view.clone(),
        }
    }
}

impl<N: Network> std::fmt::Debug for NodeProvider<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NodeProvider")
            .field(&"<dyn Provider>")
            .finish()
    }
}

//
// The rest of the file contains trait implementations for `NodeProvider` that just invoke `self.inner.<method>` inside
//

#[async_trait::async_trait]
impl<N: Network> Provider<N> for NodeProvider<N> {
    fn root(&self) -> &RootProvider<N> {
        self.inner.root()
    }

    fn client(&self) -> ClientRef<'_> {
        self.inner.client()
    }

    fn weak_client(&self) -> WeakClient {
        self.inner.weak_client()
    }

    fn get_accounts(&self) -> ProviderCall<NoParams, Vec<Address>> {
        self.inner.get_accounts()
    }

    fn get_blob_base_fee(&self) -> ProviderCall<NoParams, U128, u128> {
        self.inner.get_blob_base_fee()
    }

    fn get_block_number(&self) -> ProviderCall<NoParams, U64, BlockNumber> {
        self.inner.get_block_number()
    }

    // alloy 2.0 changed the `get_header` -> `get_block` fallback that 1.x had, so only JSON-RPC
    // errors with -32601 code from `eth_getHeaderBy*` now propagate instead of degrading to
    // `eth_getBlockBy*`. Upstream nodes return varying error codes for unsupported
    // methods, so restore the pre-2.0 behavior of falling back on any error.
    async fn get_block_number_by_id(
        &self,
        block_id: BlockId,
    ) -> TransportResult<Option<BlockNumber>> {
        match block_id {
            BlockId::Number(BlockNumberOrTag::Number(num)) => Ok(Some(num)),
            BlockId::Number(BlockNumberOrTag::Latest) => self.get_block_number().await.map(Some),
            _ => {
                if let Ok(header) = self.get_header(block_id).await {
                    return Ok(header.map(|h| h.number()));
                }
                let block = self.get_block(block_id).await?;
                Ok(block.map(|b| b.header().number()))
            }
        }
    }

    fn call(&self, tx: N::TransactionRequest) -> EthCall<N, Bytes> {
        self.inner.call(tx)
    }

    fn call_many<'req>(
        &self,
        bundles: &'req [Bundle],
    ) -> EthCallMany<'req, N, Vec<Vec<EthCallResponse>>> {
        self.inner.call_many(bundles)
    }

    fn simulate<'req>(
        &self,
        payload: &'req SimulatePayload,
    ) -> RpcWithBlock<&'req SimulatePayload, Vec<SimulatedBlock<N::BlockResponse>>> {
        self.inner.simulate(payload)
    }

    fn get_chain_id(&self) -> ProviderCall<NoParams, U64, u64> {
        self.inner.get_chain_id()
    }

    fn create_access_list<'a>(
        &self,
        request: &'a N::TransactionRequest,
    ) -> RpcWithBlock<&'a N::TransactionRequest, AccessListResult> {
        self.inner.create_access_list(request)
    }

    fn estimate_gas(&self, tx: N::TransactionRequest) -> EthCall<N, U64, u64> {
        self.inner.estimate_gas(tx)
    }

    async fn estimate_eip1559_fees_with(
        &self,
        estimator: Eip1559Estimator,
    ) -> TransportResult<Eip1559Estimation> {
        self.inner.estimate_eip1559_fees_with(estimator).await
    }

    async fn estimate_eip1559_fees(&self) -> TransportResult<Eip1559Estimation> {
        self.inner.estimate_eip1559_fees().await
    }

    async fn get_fee_history(
        &self,
        block_count: u64,
        last_block: BlockNumberOrTag,
        reward_percentiles: &[f64],
    ) -> TransportResult<FeeHistory> {
        self.inner
            .get_fee_history(block_count, last_block, reward_percentiles)
            .await
    }

    fn get_gas_price(&self) -> ProviderCall<NoParams, U128, u128> {
        self.inner.get_gas_price()
    }

    fn get_account_info(&self, address: Address) -> RpcWithBlock<Address, AccountInfo> {
        self.inner.get_account_info(address)
    }

    fn get_account(&self, address: Address) -> RpcWithBlock<Address, TrieAccount> {
        self.inner.get_account(address)
    }

    fn get_balance(&self, address: Address) -> RpcWithBlock<Address, U256, U256> {
        self.inner.get_balance(address)
    }

    fn get_block(&self, block: BlockId) -> EthGetBlock<N::BlockResponse> {
        self.inner.get_block(block)
    }

    fn get_block_by_hash(&self, hash: BlockHash) -> EthGetBlock<N::BlockResponse> {
        self.inner.get_block_by_hash(hash)
    }

    fn get_block_by_number(&self, number: BlockNumberOrTag) -> EthGetBlock<N::BlockResponse> {
        self.inner.get_block_by_number(number)
    }

    async fn get_block_transaction_count_by_hash(
        &self,
        hash: BlockHash,
    ) -> TransportResult<Option<u64>> {
        self.inner.get_block_transaction_count_by_hash(hash).await
    }

    async fn get_block_transaction_count_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> TransportResult<Option<u64>> {
        self.inner
            .get_block_transaction_count_by_number(block_number)
            .await
    }

    fn get_block_receipts(
        &self,
        block: BlockId,
    ) -> ProviderCall<(BlockId,), Option<Vec<N::ReceiptResponse>>> {
        self.inner.get_block_receipts(block)
    }

    fn get_code_at(&self, address: Address) -> RpcWithBlock<Address, Bytes> {
        self.inner.get_code_at(address)
    }

    async fn watch_blocks(&self) -> TransportResult<FilterPollerBuilder<B256>> {
        self.inner.watch_blocks().await
    }

    async fn watch_pending_transactions(&self) -> TransportResult<FilterPollerBuilder<B256>> {
        self.inner.watch_pending_transactions().await
    }

    async fn watch_logs(&self, filter: &Filter) -> TransportResult<FilterPollerBuilder<Log>> {
        self.inner.watch_logs(filter).await
    }

    async fn watch_full_pending_transactions(
        &self,
    ) -> TransportResult<FilterPollerBuilder<N::TransactionResponse>> {
        self.inner.watch_full_pending_transactions().await
    }

    async fn get_filter_changes_dyn(&self, id: U256) -> TransportResult<FilterChanges> {
        self.inner.get_filter_changes_dyn(id).await
    }

    async fn get_filter_logs(&self, id: U256) -> TransportResult<Vec<Log>> {
        self.inner.get_filter_logs(id).await
    }

    async fn uninstall_filter(&self, id: U256) -> TransportResult<bool> {
        self.inner.uninstall_filter(id).await
    }

    async fn watch_pending_transaction(
        &self,
        config: PendingTransactionConfig,
    ) -> Result<PendingTransaction, PendingTransactionError> {
        self.inner.watch_pending_transaction(config).await
    }

    async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>> {
        self.inner.get_logs(filter).await
    }

    fn get_proof(
        &self,
        address: Address,
        keys: Vec<StorageKey>,
    ) -> RpcWithBlock<(Address, Vec<StorageKey>), EIP1186AccountProofResponse> {
        self.inner.get_proof(address, keys)
    }

    fn get_storage_at(
        &self,
        address: Address,
        key: U256,
    ) -> RpcWithBlock<(Address, U256), StorageValue> {
        self.inner.get_storage_at(address, key)
    }

    fn get_transaction_by_hash(
        &self,
        hash: TxHash,
    ) -> ProviderCall<(TxHash,), Option<N::TransactionResponse>> {
        self.inner.get_transaction_by_hash(hash)
    }

    fn get_transaction_by_sender_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> ProviderCall<(Address, U64), Option<N::TransactionResponse>> {
        self.inner.get_transaction_by_sender_nonce(sender, nonce)
    }

    fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: usize,
    ) -> ProviderCall<(B256, Index), Option<N::TransactionResponse>> {
        self.inner
            .get_transaction_by_block_hash_and_index(block_hash, index)
    }

    fn get_raw_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: usize,
    ) -> ProviderCall<(B256, Index), Option<Bytes>> {
        self.inner
            .get_raw_transaction_by_block_hash_and_index(block_hash, index)
    }

    fn get_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: usize,
    ) -> ProviderCall<(BlockNumberOrTag, Index), Option<N::TransactionResponse>> {
        self.inner
            .get_transaction_by_block_number_and_index(block_number, index)
    }

    fn get_raw_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: usize,
    ) -> ProviderCall<(BlockNumberOrTag, Index), Option<Bytes>> {
        self.inner
            .get_raw_transaction_by_block_number_and_index(block_number, index)
    }

    fn get_raw_transaction_by_hash(&self, hash: TxHash) -> ProviderCall<(TxHash,), Option<Bytes>> {
        self.inner.get_raw_transaction_by_hash(hash)
    }

    fn get_transaction_count(
        &self,
        address: Address,
    ) -> RpcWithBlock<Address, U64, u64, fn(U64) -> u64> {
        self.inner.get_transaction_count(address)
    }

    fn get_transaction_receipt(
        &self,
        hash: TxHash,
    ) -> ProviderCall<(TxHash,), Option<N::ReceiptResponse>> {
        self.inner.get_transaction_receipt(hash)
    }

    async fn get_uncle(&self, tag: BlockId, idx: u64) -> TransportResult<Option<N::BlockResponse>> {
        self.inner.get_uncle(tag, idx).await
    }

    async fn get_uncle_count(&self, tag: BlockId) -> TransportResult<u64> {
        self.inner.get_uncle_count(tag).await
    }

    fn get_max_priority_fee_per_gas(&self) -> ProviderCall<NoParams, U128, u128> {
        self.inner.get_max_priority_fee_per_gas()
    }

    async fn new_block_filter(&self) -> TransportResult<U256> {
        self.inner.new_block_filter().await
    }

    async fn new_filter(&self, filter: &Filter) -> TransportResult<U256> {
        self.inner.new_filter(filter).await
    }

    async fn new_pending_transactions_filter(&self, full: bool) -> TransportResult<U256> {
        self.inner.new_pending_transactions_filter(full).await
    }

    async fn send_raw_transaction(
        &self,
        encoded_tx: &[u8],
    ) -> TransportResult<PendingTransactionBuilder<N>> {
        self.inner.send_raw_transaction(encoded_tx).await
    }

    async fn send_raw_transaction_conditional(
        &self,
        encoded_tx: &[u8],
        conditional: TransactionConditional,
    ) -> TransportResult<PendingTransactionBuilder<N>> {
        self.inner
            .send_raw_transaction_conditional(encoded_tx, conditional)
            .await
    }

    async fn send_transaction(
        &self,
        tx: N::TransactionRequest,
    ) -> TransportResult<PendingTransactionBuilder<N>> {
        self.inner.send_transaction(tx).await
    }

    async fn send_tx_envelope(
        &self,
        tx: N::TxEnvelope,
    ) -> TransportResult<PendingTransactionBuilder<N>> {
        self.inner.send_tx_envelope(tx).await
    }

    async fn send_transaction_internal(
        &self,
        tx: SendableTx<N>,
    ) -> TransportResult<PendingTransactionBuilder<N>> {
        self.inner.send_transaction_internal(tx).await
    }

    async fn sign_transaction(&self, tx: N::TransactionRequest) -> TransportResult<Bytes> {
        self.inner.sign_transaction(tx).await
    }

    fn syncing(&self) -> ProviderCall<NoParams, SyncStatus> {
        self.inner.syncing()
    }

    fn get_client_version(&self) -> ProviderCall<NoParams, String> {
        self.inner.get_client_version()
    }

    fn get_sha3(&self, data: &[u8]) -> ProviderCall<(String,), B256> {
        self.inner.get_sha3(data)
    }

    fn get_net_version(&self) -> ProviderCall<NoParams, U64, u64> {
        self.inner.get_net_version()
    }

    async fn raw_request_dyn(
        &self,
        method: Cow<'static, str>,
        params: &RawValue,
    ) -> TransportResult<Box<RawValue>> {
        self.inner.raw_request_dyn(method, params).await
    }

    fn transaction_request(&self) -> N::TransactionRequest {
        self.inner.transaction_request()
    }
}

impl<N: Network> WalletProvider<N> for NodeProvider<N>
where
    EthereumWallet: NetworkWallet<N>,
{
    type Wallet = EthereumWallet;

    fn wallet(&self) -> &EthereumWallet {
        self.inner.wallet()
    }

    fn wallet_mut(&mut self) -> &mut EthereumWallet {
        self.inner.wallet_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::rpc::types::Block;
    use alloy::transports::mock::Asserter;
    use std::borrow::Cow;

    #[tokio::test]
    async fn get_block_number_by_id_falls_back_when_get_header_errors() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .wallet(EthereumWallet::default())
            .connect_mocked_client(asserter.clone());
        let provider = NodeProvider::new(provider);

        asserter.push_failure(ErrorPayload {
            code: -39001,
            message: Cow::Borrowed("custom upstream error"),
            data: None,
        });
        let mut block: Block = Block::default();
        block.header.inner.number = 42;
        asserter.push_success(&block);

        let result = provider
            .get_block_number_by_id(BlockId::finalized())
            .await
            .expect("fallback to get_block should succeed");
        assert_eq!(result, Some(42));
        assert!(
            asserter.read_q().is_empty(),
            "both mock responses should be consumed",
        );
    }

    #[tokio::test]
    async fn settlement_layer_from_gateway_retains_zksync_view() {
        let asserter = Asserter::new();
        let gateway = NodeProvider::new(
            ProviderBuilder::new_with_network::<Zksync>()
                .wallet(EthereumWallet::default())
                .connect_mocked_client(asserter.clone()),
        );

        let mut sl = NodeProvider::from_gateway(&gateway);
        assert!(sl.is_gateway());

        // The settlement-layer view shares the transport: an RPC round-trip through the rebuilt
        // client consumes the same mock queue.
        asserter.push_success(&U64::from(42));
        assert_eq!(sl.get_block_number().await.unwrap(), 42);
        assert!(asserter.read_q().is_empty());

        // Signers registered on the settlement-layer wallet after construction are visible in
        // the downcast ZK-typed view.
        let signer = alloy::signers::local::PrivateKeySigner::random();
        let signer_address = signer.address();
        WalletProvider::wallet_mut(&mut sl).register_signer(signer);
        let zk = sl.zksync().expect("gateway SL must downcast");
        assert!(NetworkWallet::<Zksync>::has_signer_for(
            WalletProvider::wallet(&zk),
            &signer_address
        ));
    }

    #[tokio::test]
    async fn settlement_layer_from_l1_has_no_zksync_view() {
        let asserter = Asserter::new();
        let l1 = NodeProvider::new(
            ProviderBuilder::new()
                .wallet(EthereumWallet::default())
                .connect_mocked_client(asserter.clone()),
        );

        let sl = NodeProvider::from_l1(&l1);
        assert!(!sl.is_gateway());
        assert!(sl.zksync().is_none());
    }
}
