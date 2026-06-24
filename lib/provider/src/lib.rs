//! The node's canonical Ethereum-network provider.
//!
//! [`NodeProvider`] is an object-safe, wallet-capable wrapper over
//! [`alloy::providers::Provider<Ethereum>`] used everywhere the node talks to an L1, Gateway, or L2
//! RPC. On top of the plain provider it caches per-address contract deployment blocks (see
//! [`NodeProvider::deployment_block`]), so the many startup binary searches over L1 history can use
//! a tight lower bound without each rediscovering it.

mod logs_cache;
mod metrics;

use alloy::consensus::{BlockHeader, TrieAccount};
use alloy::eips::eip1559::Eip1559Estimation;
use alloy::eips::eip2930::AccessListResult;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::{BlockResponse, HeaderResponse};
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
use logs_cache::LogsCache;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OnceCell, watch};

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

/// The chain id used by a local Anvil dev node by default.
pub const ANVIL_L1_CHAIN_ID: u64 = 31337;

/// Optional RPC features the underlying provider may or may not support, probed once when the
/// [`NodeProvider`] is constructed.
#[derive(Debug, Clone, Copy)]
pub struct ProviderCapabilities {
    /// Whether the RPC understands the `finalized`/`safe` block tags. When false, finalized/safe
    /// lookups degrade to the latest block.
    pub finalized_tag: bool,
    /// Whether the RPC implements `eth_getHeaderBy*`. When false, header lookups use
    /// `eth_getBlockBy*` instead.
    pub get_header: bool,
    /// Whether the underlying node supports the EIP-7594 (Fusaka/PeerDAS) blob format.
    pub supports_eip7594: bool,
}

impl ProviderCapabilities {
    /// Probes `provider` once to determine which optional features it supports.
    async fn detect(provider: &impl Provider<Ethereum>) -> Self {
        // `latest` always exists, so a failure here means `eth_getHeaderBy*` is unsupported.
        let get_header = match provider.get_header(BlockId::latest()).await {
            Ok(_) => true,
            Err(err) => {
                tracing::info!(%err, "provider lacks eth_getHeaderBy*; using eth_getBlockBy*");
                false
            }
        };
        // Probe the finalized tag with whichever block-fetch method we just confirmed works.
        let finalized_tag = if get_header {
            provider.get_header(BlockId::finalized()).await.is_ok()
        } else {
            provider.get_block(BlockId::finalized()).await.is_ok()
        };
        if !finalized_tag {
            tracing::info!("provider lacks the `finalized` block tag; degrading to latest");
        }
        // A local Anvil dev node (detected by chain id) does not support EIP-7594 blobs yet. On any
        // probe failure we assume a real node that does support them.
        let supports_eip7594 = provider
            .get_chain_id()
            .await
            .map(|id| id != ANVIL_L1_CHAIN_ID)
            .unwrap_or(true);
        Self {
            get_header,
            finalized_tag,
            supports_eip7594,
        }
    }
}

/// Per-address cache of contract deployment blocks. Cloning a [`NodeProvider`] shares this cache
/// (it sits behind an `Arc`), so all derived contract instances and watchers resolve each address
/// at most once. Each address gets its own [`OnceCell`] so concurrent lookups for the same address
/// run the binary search exactly once and the rest await its result.
type DeploymentBlockCache = Arc<Mutex<HashMap<Address, Arc<OnceCell<u64>>>>>;
type HeaderWatcher = Arc<OnceCell<watch::Sender<<Ethereum as Network>::HeaderResponse>>>;

/// A version of `DynProvider` that exposes `wallet()` and `wallet_mut()` as defined in
/// `EthWalletProvider`. Also uses `Box` instead of `Arc` to make sure the wallets are mutable.
///
/// Carries a shared [`DeploymentBlockCache`]; see [`NodeProvider::deployment_block`].
pub struct NodeProvider {
    inner: Box<dyn EthWalletProvider + 'static>,
    capabilities: ProviderCapabilities,
    deployment_blocks: DeploymentBlockCache,
    latest_header_watcher: HeaderWatcher,
    finalized_header_watcher: HeaderWatcher,
    // Poll intervals are read-only and should not be changed after initialization
    // They are here becaue pollers are initialized lazily - if we don't need it it's not initialized.
    latest_poll_interval: Duration,
    finalized_poll_interval: Duration,
    // This is optional because only the async feature-enabled constructor should eagerly create
    // the cache and its latest-header subscription. It is stored by value rather than behind an
    // `Arc` because `LogsCache` already shares its mutable state internally; cloning a
    // `NodeProvider` simply clones / shares that internal state via `LogsCache::clone()`.
    log_cache: Option<LogsCache>,
}

impl NodeProvider {
    /// Creates a new [`NodeProvider`] by erasing the type, probing the provider once for its
    /// optional [`ProviderCapabilities`].
    pub async fn new<P>(provider: P) -> TransportResult<Self>
    where
        P: EthWalletProvider + 'static,
    {
        Self::new_with_features(provider, Duration::from_secs(1), Duration::from_secs(1), 0).await
    }

    /// Creates a new [`NodeProvider`] with provider-owned pollers and, optionally, a log cache.
    pub async fn new_with_features<P>(
        provider: P,
        latest_poll_interval: Duration,
        finalized_poll_interval: Duration,
        log_cache_capacity: usize,
    ) -> TransportResult<Self>
    where
        P: EthWalletProvider + 'static,
    {
        let capabilities = ProviderCapabilities::detect(&provider).await;
        let mut this = Self {
            inner: Box::new(provider),
            capabilities,
            deployment_blocks: Arc::new(Mutex::new(HashMap::new())),
            latest_header_watcher: Arc::new(OnceCell::new()),
            finalized_header_watcher: Arc::new(OnceCell::new()),
            latest_poll_interval,
            finalized_poll_interval,
            log_cache: None,
        };

        if log_cache_capacity > 0 {
            let chain_id = this.inner.get_chain_id().await?;
            let latest_blocks = this.latest_header_watcher().await;
            this.log_cache = Some(LogsCache::new(latest_blocks, log_cache_capacity, chain_id));
        }

        Ok(this)
    }

    /// Returns a shared watcher for the latest block header via `eth_getBlockByNumber(latest, false)`.
    pub async fn latest_header_watcher(
        &self,
    ) -> watch::Receiver<<Ethereum as Network>::HeaderResponse> {
        self.latest_header_watcher
            .get_or_init(|| async {
                self.build_header_watcher(BlockNumberOrTag::Latest, self.latest_poll_interval)
                    .await
            })
            .await
            .subscribe()
    }

    /// Returns a shared watcher for the finalized block header via
    /// `eth_getBlockByNumber(finalized, false)`.
    /// Falls back to latetst if the chain does not support finalized tag.
    pub async fn finalized_header_watcher(
        &self,
    ) -> watch::Receiver<<Ethereum as Network>::HeaderResponse> {
        let finalized = if self.capabilities.finalized_tag {
            BlockNumberOrTag::Finalized
        } else {
            BlockNumberOrTag::Latest
        };
        self.finalized_header_watcher
            .get_or_init(|| async {
                self.build_header_watcher(finalized, self.finalized_poll_interval)
                    .await
            })
            .await
            .subscribe()
    }

    /// Builds a provider-owned header watcher backed by a raw RPC client request.
    ///
    /// This uses the underlying RPC client directly so the spawned task can be tied to
    /// `WeakClient` shutdown. That preserves the client's transport/request layers, but it
    /// intentionally bypasses provider-level fillers/layers.
    ///
    /// The shutdown is not tied to reth-tasks, it is only tied to the Provider. But it should be
    /// fine because the task does not own any resources. This is similar to how alloy pollers work.
    async fn build_header_watcher(
        &self,
        block: BlockNumberOrTag,
        poll_interval: Duration,
    ) -> watch::Sender<<Ethereum as Network>::HeaderResponse> {
        let initial_block: Option<<Ethereum as Network>::BlockResponse> = self
            .client()
            .request("eth_getBlockByNumber", (block, false))
            .await
            .unwrap_or_else(|err| panic!("failed to initialize {block:?} header watcher: {err}"));
        let (tx, _) = watch::channel(
            initial_block
                .expect("header watcher RPC returned no block for a chain head")
                .header()
                .clone(),
        );
        let weak_client = self.weak_client();
        let tx_task = tx.clone();

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(poll_interval);
            loop {
                timer.tick().await;
                let Some(client) = weak_client.upgrade() else {
                    return;
                };

                let block: Option<<Ethereum as Network>::BlockResponse> = client
                    .request("eth_getBlockByNumber", (block, false))
                    .await
                    .unwrap_or_else(|err| {
                        panic!("failed to poll {block:?} header watcher: {err}");
                    });
                let header = block
                    .expect("header watcher RPC returned no block for a chain head")
                    .header()
                    .clone();
                tx_task.send_if_modified(|current: &mut <Ethereum as Network>::HeaderResponse| {
                    if current.hash() == header.hash() {
                        false
                    } else {
                        *current = header.clone();
                        true
                    }
                });
            }
        });

        tx
    }

    /// Returns the optional features the underlying provider was detected to support.
    pub fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
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

impl Clone for NodeProvider {
    fn clone(&self) -> Self {
        NodeProvider {
            inner: self.inner.dyn_clone(),
            capabilities: self.capabilities,
            deployment_blocks: self.deployment_blocks.clone(),
            latest_header_watcher: self.latest_header_watcher.clone(),
            finalized_header_watcher: self.finalized_header_watcher.clone(),
            latest_poll_interval: self.latest_poll_interval,
            finalized_poll_interval: self.finalized_poll_interval,
            log_cache: self.log_cache.clone(),
        }
    }
}

impl std::fmt::Debug for NodeProvider {
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
impl Provider<Ethereum> for NodeProvider {
    fn root(&self) -> &RootProvider<Ethereum> {
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

    // Dispatch based on the capabilities probed at construction (see `ProviderCapabilities`)
    // instead of trying-and-failing: skip `eth_getHeaderBy*` when unsupported, and degrade
    // finalized/safe lookups straight to the latest block when the tags are unsupported.
    async fn get_block_number_by_id(
        &self,
        block_id: BlockId,
    ) -> TransportResult<Option<BlockNumber>> {
        match block_id {
            BlockId::Number(BlockNumberOrTag::Number(num)) => Ok(Some(num)),
            BlockId::Number(BlockNumberOrTag::Latest) => self.get_block_number().await.map(Some),
            _ => {
                if (block_id.is_finalized() || block_id.is_safe())
                    && !self.capabilities.finalized_tag
                {
                    // If `finalized`/`safe` are not supported we presume immediate finality like it
                    // is with Besu: https://besu.hyperledger.org/private-networks/concepts/poa
                    return self.get_block_number().await.map(Some);
                }
                if self.capabilities.get_header {
                    Ok(self.get_header(block_id).await?.map(|h| h.number()))
                } else {
                    Ok(self.get_block(block_id).await?.map(|b| b.header().number()))
                }
            }
        }
    }

    fn call(&self, tx: <Ethereum as Network>::TransactionRequest) -> EthCall<Ethereum, Bytes> {
        self.inner.call(tx)
    }

    fn call_many<'req>(
        &self,
        bundles: &'req [Bundle],
    ) -> EthCallMany<'req, Ethereum, Vec<Vec<EthCallResponse>>> {
        self.inner.call_many(bundles)
    }

    fn simulate<'req>(
        &self,
        payload: &'req SimulatePayload,
    ) -> RpcWithBlock<
        &'req SimulatePayload,
        Vec<SimulatedBlock<<Ethereum as Network>::BlockResponse>>,
    > {
        self.inner.simulate(payload)
    }

    fn get_chain_id(&self) -> ProviderCall<NoParams, U64, u64> {
        self.inner.get_chain_id()
    }

    fn create_access_list<'a>(
        &self,
        request: &'a <Ethereum as Network>::TransactionRequest,
    ) -> RpcWithBlock<&'a <Ethereum as Network>::TransactionRequest, AccessListResult> {
        self.inner.create_access_list(request)
    }

    fn estimate_gas(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> EthCall<Ethereum, U64, u64> {
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

    fn get_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.inner.get_block(block)
    }

    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.inner.get_block_by_hash(hash)
    }

    fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
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
    ) -> ProviderCall<(BlockId,), Option<Vec<<Ethereum as Network>::ReceiptResponse>>> {
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
    ) -> TransportResult<FilterPollerBuilder<<Ethereum as Network>::TransactionResponse>> {
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
        if let Some(log_cache) = &self.log_cache {
            log_cache.get_logs(self.inner.root(), filter).await
        } else {
            self.inner.get_logs(filter).await
        }
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
    ) -> ProviderCall<(TxHash,), Option<<Ethereum as Network>::TransactionResponse>> {
        self.inner.get_transaction_by_hash(hash)
    }

    fn get_transaction_by_sender_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> ProviderCall<(Address, U64), Option<<Ethereum as Network>::TransactionResponse>> {
        self.inner.get_transaction_by_sender_nonce(sender, nonce)
    }

    fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: usize,
    ) -> ProviderCall<(B256, Index), Option<<Ethereum as Network>::TransactionResponse>> {
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
    ) -> ProviderCall<(BlockNumberOrTag, Index), Option<<Ethereum as Network>::TransactionResponse>>
    {
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
    ) -> ProviderCall<(TxHash,), Option<<Ethereum as Network>::ReceiptResponse>> {
        self.inner.get_transaction_receipt(hash)
    }

    async fn get_uncle(
        &self,
        tag: BlockId,
        idx: u64,
    ) -> TransportResult<Option<<Ethereum as Network>::BlockResponse>> {
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
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.inner.send_raw_transaction(encoded_tx).await
    }

    async fn send_raw_transaction_conditional(
        &self,
        encoded_tx: &[u8],
        conditional: TransactionConditional,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.inner
            .send_raw_transaction_conditional(encoded_tx, conditional)
            .await
    }

    async fn send_transaction(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.inner.send_transaction(tx).await
    }

    async fn send_tx_envelope(
        &self,
        tx: <Ethereum as Network>::TxEnvelope,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.inner.send_tx_envelope(tx).await
    }

    async fn send_transaction_internal(
        &self,
        tx: SendableTx<Ethereum>,
    ) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
        self.inner.send_transaction_internal(tx).await
    }

    async fn sign_transaction(
        &self,
        tx: <Ethereum as Network>::TransactionRequest,
    ) -> TransportResult<Bytes> {
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

    fn transaction_request(&self) -> <Ethereum as Network>::TransactionRequest {
        self.inner.transaction_request()
    }
}

impl EthWalletProvider for NodeProvider {
    fn dyn_clone(&self) -> Box<dyn EthWalletProvider> {
        self.inner.dyn_clone()
    }

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
    use alloy::rpc::types::{Block, Header};
    use alloy::transports::mock::Asserter;
    use std::borrow::Cow;

    fn mocked_provider(asserter: &Asserter) -> impl EthWalletProvider {
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .wallet(EthereumWallet::default())
            .connect_mocked_client(asserter.clone())
    }

    fn header_with_number(number: u64) -> Header {
        let mut block: Block = Block::default();
        block.header.inner.number = number;
        block.header
    }

    fn block_with_number(number: u64) -> Block {
        let mut block: Block = Block::default();
        block.header.inner.number = number;
        block
    }

    fn unsupported_method() -> ErrorPayload {
        ErrorPayload {
            code: -39001,
            message: Cow::Borrowed("custom upstream error"),
            data: None,
        }
    }

    #[tokio::test]
    async fn uses_get_header_when_supported() {
        let asserter = Asserter::new();
        // Probe: get_header(latest) ok -> get_header supported; get_header(finalized) ok ->
        // finalized supported; get_chain_id -> non-anvil.
        asserter.push_success(&header_with_number(1));
        asserter.push_success(&header_with_number(1));
        asserter.push_success(&U64::from(1));
        let provider = NodeProvider::new(mocked_provider(&asserter))
            .await
            .expect("mocked provider construction should succeed");
        assert!(provider.capabilities().get_header);
        assert!(provider.capabilities().finalized_tag);
        assert!(provider.capabilities().supports_eip7594);

        // The lookup itself resolves via get_header.
        asserter.push_success(&header_with_number(42));
        let result = provider
            .get_block_number_by_id(BlockId::finalized())
            .await
            .expect("get_header lookup should succeed");
        assert_eq!(result, Some(42));
        assert!(asserter.read_q().is_empty(), "all responses consumed");
    }

    #[tokio::test]
    async fn falls_back_to_get_block_when_get_header_unsupported() {
        let asserter = Asserter::new();
        // Probe: get_header(latest) fails -> get_header unsupported; get_block(finalized) ok ->
        // finalized supported; get_chain_id -> non-anvil.
        asserter.push_failure(unsupported_method());
        asserter.push_success(&block_with_number(1));
        asserter.push_success(&U64::from(1));
        let provider = NodeProvider::new(mocked_provider(&asserter))
            .await
            .expect("mocked provider construction should succeed");
        assert!(!provider.capabilities().get_header);
        assert!(provider.capabilities().finalized_tag);
        assert!(provider.capabilities().supports_eip7594);

        // The lookup resolves via get_block, never touching get_header.
        asserter.push_success(&block_with_number(42));
        let result = provider
            .get_block_number_by_id(BlockId::finalized())
            .await
            .expect("get_block lookup should succeed");
        assert_eq!(result, Some(42));
        assert!(asserter.read_q().is_empty(), "all responses consumed");
    }

    #[tokio::test]
    async fn degrades_to_latest_when_finalized_unsupported() {
        let asserter = Asserter::new();
        // Probe: get_header(latest) ok -> get_header supported; get_header(finalized) fails ->
        // finalized unsupported; get_chain_id -> non-anvil.
        asserter.push_success(&header_with_number(1));
        asserter.push_failure(unsupported_method());
        asserter.push_success(&U64::from(1));
        let provider = NodeProvider::new(mocked_provider(&asserter))
            .await
            .expect("mocked provider construction should succeed");
        assert!(provider.capabilities().get_header);
        assert!(!provider.capabilities().finalized_tag);
        assert!(provider.capabilities().supports_eip7594);

        // The lookup degrades straight to get_block_number (latest), issuing no header/block call.
        asserter.push_success(&U64::from(99));
        let result = provider
            .get_block_number_by_id(BlockId::finalized())
            .await
            .expect("degraded latest lookup should succeed");
        assert_eq!(result, Some(99));
        assert!(asserter.read_q().is_empty(), "all responses consumed");
    }

    #[tokio::test]
    async fn anvil_chain_id_disables_eip7594() {
        let asserter = Asserter::new();
        // Probe: get_header(latest) ok; get_header(finalized) ok; get_chain_id -> anvil.
        asserter.push_success(&header_with_number(1));
        asserter.push_success(&header_with_number(1));
        asserter.push_success(&U64::from(ANVIL_L1_CHAIN_ID));
        let provider = NodeProvider::new(mocked_provider(&asserter))
            .await
            .expect("mocked provider construction should succeed");
        assert!(!provider.capabilities().supports_eip7594);
        assert!(asserter.read_q().is_empty(), "all responses consumed");
    }
}
