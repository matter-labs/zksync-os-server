use crate::TxGasRateLimitConfig;
use crate::metrics::TX_GAS_RATE_LIMITER;
use crate::subpools::l2::L2Subpool;
use alloy::consensus::BlockHeader;
use alloy::primitives::Address;
use reth_transaction_pool::error::PoolError;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStoreError, BlockInfo, CanonicalStateUpdate,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, PoolResult, PoolSize,
    PoolTransaction, PropagatedTransactions, TransactionEvents, TransactionListenerKind,
    TransactionOrigin, TransactionPool, TransactionPoolExt, ValidPoolTransaction,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use zksync_os_types::{FeeParams, ProtocolSemanticVersion};

/// Rate limiter for incoming L2 transactions, gating admission based on the sequencer's
/// total recent execution throughput.
///
/// A shared "gas bank" is drained by each sealed block's executed gas and refilled by
/// wall-clock time at `gas_per_second`. The gate closes when the bank is exhausted and
/// reopens once it recovers `reopen_credit`. Non-obvious properties:
/// - The bank goes negative down to `-deficit_floor`: overshoot is repaid before
///   reopening, keeping the long-run executed average at `gas_per_second`.
/// - The drain is block-granular; overshoot admitted within a block self-corrects via
///   the deficit. There is no per-transaction bookkeeping.
/// - "Executed gas" is the block's total, not just L2: L1 priority/upgrade/interop txs
///   drain the same bank though they're never gated. Intentional — it's the executor's
///   real capacity being protected — but means non-L2 traffic can close the gate for L2 users.
#[derive(Debug)]
pub(crate) struct TxGasRateLimiter {
    /// Refill rate, gas per second.
    rate: f64,
    /// Bank capacity: idle burst headroom, gas.
    max_credit: f64,
    /// Bank level required to reopen the gate, gas.
    reopen_credit: f64,
    /// Lowest allowed bank level (`<= 0`): max remembered deficit, gas.
    floor: f64,
    exempt_senders: HashSet<Address>,
    bank: Mutex<Bank>,
}

#[derive(Debug)]
struct Bank {
    level: f64,
    last_refill: Instant,
    gate_open: bool,
}

impl TxGasRateLimiter {
    pub(crate) fn new(config: &TxGasRateLimitConfig) -> Self {
        let rate = config.gas_per_second as f64;
        let limiter = Self {
            rate,
            max_credit: config.max_credit_seconds * rate,
            reopen_credit: config.reopen_credit_seconds * rate,
            floor: -(config.deficit_floor_seconds * rate),
            exempt_senders: config.exempt_senders.clone(),
            bank: Mutex::new(Bank {
                level: config.max_credit_seconds * rate,
                last_refill: Instant::now(),
                gate_open: true,
            }),
        };
        TX_GAS_RATE_LIMITER.gate_open.set(1);
        TX_GAS_RATE_LIMITER
            .bank_level_gas
            .set(limiter.max_credit as i64);
        limiter
    }

    pub(crate) fn is_exempt(&self, sender: &Address) -> bool {
        self.exempt_senders.contains(sender)
    }

    /// On rejection returns a suggested retry delay: a lower bound until the gate can
    /// reopen, jittered upwards so synchronized clients don't stampede the reopen instant.
    pub(crate) fn try_admit(&self) -> Result<(), Duration> {
        self.try_admit_at(Instant::now(), rand::random::<f64>)
    }

    fn try_admit_at(&self, now: Instant, jitter: impl FnOnce() -> f64) -> Result<(), Duration> {
        let mut bank = self.bank.lock().unwrap();
        self.refill(&mut bank, now);
        self.update_gate(&mut bank);
        // Every admission attempt already recomputes the refilled level; publish it here
        // too (not just from `on_block_at`) so the gauge stays live between blocks instead
        // of only jumping on drain.
        TX_GAS_RATE_LIMITER.bank_level_gas.set(bank.level as i64);
        if bank.gate_open {
            Ok(())
        } else {
            let base_secs = ((self.reopen_credit - bank.level) / self.rate).max(0.0);
            // Misconfig guard to keep `from_secs_f64` panic-free (since `f64::min` discards
            // NaN/inf in favor of the other operand).
            let secs = (base_secs * (1.0 + jitter() * 0.5)).min(300.0);
            Err(Duration::from_secs_f64(secs))
        }
    }

    pub(crate) fn on_block(&self, block_gas_used: u64) {
        self.on_block_at(block_gas_used, Instant::now())
    }

    fn on_block_at(&self, block_gas_used: u64, now: Instant) {
        let mut bank = self.bank.lock().unwrap();
        self.refill(&mut bank, now);
        bank.level = (bank.level - block_gas_used as f64).max(self.floor);
        self.update_gate(&mut bank);
        TX_GAS_RATE_LIMITER.bank_level_gas.set(bank.level as i64);
    }

    fn refill(&self, bank: &mut Bank, now: Instant) {
        let elapsed = now.saturating_duration_since(bank.last_refill);
        bank.last_refill = now;
        bank.level = (bank.level + elapsed.as_secs_f64() * self.rate).min(self.max_credit);
    }

    fn update_gate(&self, bank: &mut Bank) {
        if bank.gate_open && bank.level <= 0.0 {
            bank.gate_open = false;
            tracing::warn!(
                bank_level_gas = bank.level as i64,
                reopen_credit_gas = self.reopen_credit as i64,
                "tx gas rate limiter: bank exhausted, suspending acceptance of non-exempt transactions"
            );
            TX_GAS_RATE_LIMITER.gate_closes.inc();
            TX_GAS_RATE_LIMITER.gate_open.set(0);
        } else if !bank.gate_open && bank.level >= self.reopen_credit {
            bank.gate_open = true;
            tracing::info!(
                bank_level_gas = bank.level as i64,
                "tx gas rate limiter: bank recovered, resuming acceptance"
            );
            TX_GAS_RATE_LIMITER.gate_open.set(1);
        }
    }
}

/// Carries the suggested retry delay through a [`PoolError`]'s [`PoolErrorKind::Other`]
/// slot (see [`gas_rate_limit_retry_after`]).
///
/// [`PoolErrorKind::Other`]: reth_transaction_pool::error::PoolErrorKind::Other
#[derive(Debug, thiserror::Error)]
#[error("executed-gas rate limiter: bank exhausted, retry in ~{retry_after:?}")]
struct GasRateLimited {
    retry_after: Duration,
}

/// Extracts the rate limiter's suggested retry delay from a rejection returned by
/// [`RateLimitedL2Subpool::add_transaction`], if that's what caused it.
pub fn gas_rate_limit_retry_after(err: &PoolError) -> Option<Duration> {
    match &err.kind {
        reth_transaction_pool::error::PoolErrorKind::Other(other) => other
            .downcast_ref::<GasRateLimited>()
            .map(|e| e.retry_after),
        _ => None,
    }
}

/// Decorates an [`L2Subpool`] with the executed-gas rate limiter: gates [`Self::add_transaction`]
/// (the single choke point every admission path — local RPC today, gossip once consensus lands —
/// funnels through) and drains the bank from [`Self::on_canonical_state_change`], which this node
/// already receives for every block it commits or replays — including blocks made up entirely
/// of transaction types this gate never sees, see [`TxGasRateLimiter`]'s doc.
///
/// `limiter: None` makes this a transparent passthrough, used on nodes/configs where the feature
/// is disabled (see [`crate::subpools::l2::in_memory`]).
#[derive(Clone, Debug)]
pub(crate) struct RateLimitedL2Subpool<T> {
    inner: T,
    limiter: Option<Arc<TxGasRateLimiter>>,
    /// None until `arm_gas_rate_limiter` runs — skip draining until then, since replay/
    /// catch-up blocks arrive at replay speed. Shared across clones so arming one arms all.
    started_at: Arc<std::sync::OnceLock<u64>>,
}

impl<T> RateLimitedL2Subpool<T> {
    pub(crate) fn new(inner: T, limiter: Option<Arc<TxGasRateLimiter>>) -> Self {
        Self {
            inner,
            limiter,
            started_at: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// No bound on `T`: lets tests exercise the gating/draining decisions directly,
    /// without needing a real (or fake) `TransactionPool` to construct one.
    fn arm(&self) {
        if self.limiter.is_some() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Only the first arm call sets it; harmless if called more than once.
            let _ = self.started_at.set(now);
        }
    }

    /// The rejection this sender's transaction incurs right now, if any (`None` = admit).
    fn gas_rate_limit_rejection(&self, sender: Address) -> Option<Duration> {
        let limiter = self.limiter.as_ref()?;
        if limiter.is_exempt(&sender) {
            return None;
        }
        limiter.try_admit().err()
    }

    /// The limiter to drain against a block with this timestamp, if armed and the block
    /// postdates arming.
    fn limiter_to_drain(&self, block_timestamp: u64) -> Option<&TxGasRateLimiter> {
        let limiter = self.limiter.as_ref()?;
        let &started_at = self.started_at.get()?;
        (block_timestamp >= started_at).then_some(limiter.as_ref())
    }
}

impl<T: L2Subpool> L2Subpool for RateLimitedL2Subpool<T> {
    fn arm_gas_rate_limiter(&self) {
        self.arm();
    }

    fn update_pending_fee_params(&self, fee_params: FeeParams) {
        self.inner.update_pending_fee_params(fee_params)
    }

    fn update_pending_protocol_version(&self, protocol_version: ProtocolSemanticVersion) {
        self.inner.update_pending_protocol_version(protocol_version)
    }
}

impl<T: TransactionPool> TransactionPool for RateLimitedL2Subpool<T> {
    type Transaction = T::Transaction;

    fn pool_size(&self) -> PoolSize {
        self.inner.pool_size()
    }

    fn block_info(&self) -> BlockInfo {
        self.inner.block_info()
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        self.inner
            .add_transaction_and_subscribe(origin, transaction)
            .await
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        if let Some(retry_after) = self.gas_rate_limit_rejection(transaction.sender()) {
            return Err(PoolError::other(
                *transaction.hash(),
                GasRateLimited { retry_after },
            ));
        }
        self.inner.add_transaction(origin, transaction).await
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        self.inner.add_transactions(origin, transactions).await
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        self.inner.add_transactions_with_origins(transactions).await
    }

    fn transaction_event_listener(
        &self,
        tx_hash: alloy::primitives::TxHash,
    ) -> Option<TransactionEvents> {
        self.inner.transaction_event_listener(tx_hash)
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        self.inner.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<alloy::primitives::TxHash> {
        self.inner.pending_transactions_listener_for(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        self.inner.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        self.inner.new_transactions_listener_for(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<alloy::primitives::TxHash> {
        self.inner.pooled_transaction_hashes()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<alloy::primitives::TxHash> {
        self.inner.pooled_transaction_hashes_max(max)
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions()
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions_max(max)
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<alloy::primitives::TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        self.inner.get_pooled_transaction_elements(tx_hashes, limit)
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: alloy::primitives::TxHash,
    ) -> Option<reth_primitives_traits::Recovered<<Self::Transaction as PoolTransaction>::Pooled>>
    {
        self.inner.get_pooled_transaction_element(tx_hash)
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner.best_transactions()
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner
            .best_transactions_with_attributes(best_transactions_attributes)
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions()
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions_max(max)
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.queued_transactions()
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        self.inner.pending_and_queued_txn_count()
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        self.inner.all_transactions()
    }

    fn all_transaction_hashes(&self) -> Vec<alloy::primitives::TxHash> {
        self.inner.all_transaction_hashes()
    }

    fn remove_transactions(
        &self,
        hashes: Vec<alloy::primitives::TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions(hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<alloy::primitives::TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions_by_sender(sender)
    }

    fn prune_transactions(
        &self,
        hashes: Vec<alloy::primitives::TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.prune_transactions(hashes)
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: reth_eth_wire_types::HandleMempoolData,
    {
        self.inner.retain_unknown(announcement)
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: reth_eth_wire_types::HandleMempoolData,
    {
        self.inner.retain_contains(announcement)
    }

    fn get(
        &self,
        tx_hash: &alloy::primitives::TxHash,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get(tx_hash)
    }

    fn get_all(
        &self,
        txs: Vec<alloy::primitives::TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_all(txs)
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.inner.on_propagated(txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_pending_transactions_with_predicate(predicate)
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_origin(origin)
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> alloy::primitives::map::AddressSet {
        self.inner.unique_senders()
    }

    fn get_blob(
        &self,
        tx_hash: alloy::primitives::TxHash,
    ) -> Result<Option<Arc<alloy::eips::eip7594::BlobTransactionSidecarVariant>>, BlobStoreError>
    {
        self.inner.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<alloy::primitives::TxHash>,
    ) -> Result<
        Vec<(
            alloy::primitives::TxHash,
            Arc<alloy::eips::eip7594::BlobTransactionSidecarVariant>,
        )>,
        BlobStoreError,
    > {
        self.inner.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<alloy::primitives::TxHash>,
    ) -> Result<Vec<Arc<alloy::eips::eip7594::BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.inner.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[alloy::primitives::B256],
    ) -> Result<Vec<Option<alloy::eips::eip4844::BlobAndProofV1>>, BlobStoreError> {
        self.inner
            .get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[alloy::primitives::B256],
    ) -> Result<Option<Vec<alloy::eips::eip4844::BlobAndProofV2>>, BlobStoreError> {
        self.inner
            .get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[alloy::primitives::B256],
    ) -> Result<Vec<Option<alloy::eips::eip4844::BlobAndProofV2>>, BlobStoreError> {
        self.inner
            .get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[alloy::primitives::B256],
        indices_bitarray: alloy::primitives::B128,
    ) -> Result<Vec<Option<alloy::eips::eip4844::BlobCellsAndProofsV1>>, BlobStoreError> {
        self.inner
            .get_blobs_for_versioned_hashes_v4(versioned_hashes, indices_bitarray)
    }

    fn blob_store(&self) -> Box<dyn reth_transaction_pool::blobstore::BlobStore> {
        self.inner.blob_store()
    }
}

impl<T: TransactionPoolExt> TransactionPoolExt for RateLimitedL2Subpool<T> {
    type Block = T::Block;

    fn set_block_info(&self, info: BlockInfo) {
        self.inner.set_block_info(info)
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_, Self::Block>) {
        if let Some(limiter) = self.limiter_to_drain(update.new_tip.header().timestamp()) {
            limiter.on_block(update.new_tip.header().gas_used());
        }
        self.inner.on_canonical_state_change(update);
    }

    fn update_accounts(&self, accounts: Vec<reth_execution_types::ChangedAccount>) {
        self.inner.update_accounts(accounts)
    }

    fn delete_blob(&self, tx: alloy::primitives::B256) {
        self.inner.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<alloy::primitives::B256>) {
        self.inner.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.inner.cleanup_blobs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> TxGasRateLimiter {
        // rate 100k gas/s, max credit 200k, reopen at 100k, floor at -200k
        TxGasRateLimiter::new(&TxGasRateLimitConfig {
            gas_per_second: 100_000,
            max_credit_seconds: 2.0,
            reopen_credit_seconds: 1.0,
            deficit_floor_seconds: 2.0,
            exempt_senders: HashSet::from([Address::repeat_byte(0xaa)]),
        })
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn starts_open_with_full_credit() {
        let l = limiter();
        let t0 = Instant::now();
        assert!(l.try_admit_at(t0, || 0.0).is_ok());
        // Draining just under the full credit keeps the gate open.
        l.on_block_at(199_999, t0);
        assert!(l.try_admit_at(t0, || 0.0).is_ok());
    }

    #[test]
    fn closes_when_bank_exhausted_and_reports_retry_after() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        // level = 0 → closed; recovery to reopen_credit (100k) takes 1s at 100k/s.
        let retry = l.try_admit_at(t0, || 0.0).unwrap_err();
        assert_eq!(retry, secs(1.0));
    }

    #[test]
    fn hysteresis_keeps_gate_closed_until_reopen_credit() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        assert!(l.try_admit_at(t0, || 0.0).is_err());
        // 0.99s later the bank is at 99k, just below the 100k reopen threshold.
        assert!(l.try_admit_at(t0 + secs(0.99), || 0.0).is_err());
        assert!(l.try_admit_at(t0 + secs(1.0), || 0.0).is_ok());
        // Once open, it stays open even though the level is below reopen_credit.
        l.on_block_at(50_000, t0 + secs(1.0));
        assert!(l.try_admit_at(t0 + secs(1.0), || 0.0).is_ok());
    }

    #[test]
    fn deficit_is_remembered_down_to_floor_and_repaid() {
        let l = limiter();
        let t0 = Instant::now();
        // Massive overshoot: bank clamps at the floor (-200k), not below.
        l.on_block_at(10_000_000, t0);
        // Recovery from -200k to +100k takes 3s at 100k/s.
        let retry = l.try_admit_at(t0, || 0.0).unwrap_err();
        assert_eq!(retry, secs(3.0));
        assert!(l.try_admit_at(t0 + secs(2.99), || 0.0).is_err());
        assert!(l.try_admit_at(t0 + secs(3.0), || 0.0).is_ok());
    }

    #[test]
    fn zero_floor_clamps_bank_at_zero() {
        let l = TxGasRateLimiter::new(&TxGasRateLimitConfig {
            gas_per_second: 100_000,
            max_credit_seconds: 2.0,
            reopen_credit_seconds: 1.0,
            deficit_floor_seconds: 0.0,
            exempt_senders: HashSet::new(),
        });
        let t0 = Instant::now();
        l.on_block_at(10_000_000, t0);
        // No deficit remembered: recovery is reopen_credit / rate regardless of overshoot.
        assert_eq!(l.try_admit_at(t0, || 0.0).unwrap_err(), secs(1.0));
    }

    #[test]
    fn refill_caps_at_max_credit() {
        let l = limiter();
        let t0 = Instant::now();
        // A long idle period must not accumulate more than max_credit (200k):
        // draining exactly max_credit afterwards empties the bank and closes the gate.
        l.on_block_at(0, t0 + secs(100.0));
        l.on_block_at(200_000, t0 + secs(100.0));
        assert!(l.try_admit_at(t0 + secs(100.0), || 0.0).is_err());
    }

    #[test]
    fn retry_after_is_jittered_upwards() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        // Base retry is 1s; jitter=1.0 stretches it by 50%.
        assert_eq!(l.try_admit_at(t0, || 1.0).unwrap_err(), secs(1.5));
    }

    #[test]
    fn exempt_senders_are_recognized() {
        let l = limiter();
        assert!(l.is_exempt(&Address::repeat_byte(0xaa)));
        assert!(!l.is_exempt(&Address::repeat_byte(0xbb)));
    }

    // `RateLimitedL2Subpool<()>` below: `new`/`arm`/`gas_rate_limit_rejection`/`limiter_to_drain`
    // carry no bound on `T`, so a real (or fake) `TransactionPool` isn't needed to exercise them.

    #[test]
    fn exempt_sender_bypasses_the_gate_even_when_closed() {
        let l = limiter();
        l.on_block_at(200_000, Instant::now()); // exhausts the bank, closing the gate
        let pool = RateLimitedL2Subpool::new((), Some(Arc::new(l)));
        assert!(
            pool.gas_rate_limit_rejection(Address::repeat_byte(0xaa))
                .is_none()
        );
        assert!(
            pool.gas_rate_limit_rejection(Address::repeat_byte(0xbb))
                .is_some()
        );
    }

    #[test]
    fn disabled_limiter_never_rejects_or_drains() {
        let pool: RateLimitedL2Subpool<()> = RateLimitedL2Subpool::new((), None);
        pool.arm();
        assert!(
            pool.gas_rate_limit_rejection(Address::repeat_byte(0xbb))
                .is_none()
        );
        assert!(pool.limiter_to_drain(u64::MAX).is_none());
    }

    #[test]
    fn drain_applies_only_to_blocks_sealed_after_arming() {
        let pool = RateLimitedL2Subpool::new((), Some(Arc::new(limiter())));
        // Not armed yet: even a block far in the "future" must not drain.
        assert!(pool.limiter_to_drain(u64::MAX).is_none());
        pool.arm();
        // Armed: a block sealed after arming drains...
        assert!(pool.limiter_to_drain(u64::MAX).is_some());
        // ...but one that predates arming (WAL replay, EN catch-up) must not.
        assert!(pool.limiter_to_drain(0).is_none());
    }
}
