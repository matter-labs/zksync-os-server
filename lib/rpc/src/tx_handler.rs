use crate::eth_call_handler::build_pending_block_context;
use crate::eth_impl::build_api_receipt;
use crate::metrics::{TX_SUBMISSION, TxRejectionReason};
use crate::tx_forwarder::{TxForwardError, TxForwarder};
use crate::{ReadRpcStorage, RpcConfig};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::transports::RpcError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_mempool::PoolError;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{InvalidPoolTransactionError, PoolErrorKind};
use zksync_os_rpc_api::types::ZkTransactionReceipt;
use zksync_os_storage_api::BlockContext;
use zksync_os_tx_validators::policy_client::{AccessType, PolicyClient};
use zksync_os_types::{
    L2Envelope, L2Transaction, NotAcceptingReason, TransactionAcceptanceState, ZkTransaction,
};

/// Maximum user provided timeout for `eth_sendRawTransactionSync`. Chosen liberally as waiting is
/// inexpensive.
const SEND_RAW_TRANSACTION_SYNC_MAX_TIMEOUT: Duration = Duration::from_secs(30);

/// JSON-RPC error code used by EIP-7966 to signal a sync-send timeout.
const EIP_7966_TIMEOUT_CODE: i64 = 4;

/// Counts samples, not admissions: 100k samples ≈ 6.4M txs between log lines.
const ADMISSION_PROFILE_LOG_EVERY: u64 = 1_000_000;

/// Record only every Nth admission per worker thread. Averages stay unbiased (admission
/// latencies carry no period-64 structure) while an unsampled admission costs one
/// thread-local increment + branch — no shared-cacheline RMWs, no clock reads.
const ADMISSION_SAMPLE_EVERY: u64 = 64;

thread_local! {
    static ADMISSION_SAMPLE_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

static ADMISSION_PROFILE_ENABLED: OnceLock<bool> = OnceLock::new();
static ADMISSION_PROFILE_STATS: AdmissionProfileStats = AdmissionProfileStats::new();

struct AdmissionProfileStats {
    total: AtomicU64,
    direct: AtomicU64,
    // Sampled-guard concurrency (≈ actual admissions in flight / ADMISSION_SAMPLE_EVERY);
    // log-only diagnostics, not exported.
    in_flight: AtomicU64,
    max_in_flight: AtomicU64,
    // Nanos, not micros: decode/hash_signer run ~1µs, so per-sample micros truncation
    // would bias them down by up to ~50%. u64 nanos holds ~584 years of accumulated time.
    decode_nanos: AtomicU64,
    recover_nanos: AtomicU64,
    hash_signer_nanos: AtomicU64,
    lane_send_nanos: AtomicU64,
    total_nanos: AtomicU64,
}

impl AdmissionProfileStats {
    const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            direct: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            max_in_flight: AtomicU64::new(0),
            decode_nanos: AtomicU64::new(0),
            recover_nanos: AtomicU64::new(0),
            hash_signer_nanos: AtomicU64::new(0),
            lane_send_nanos: AtomicU64::new(0),
            total_nanos: AtomicU64::new(0),
        }
    }
}

fn admission_profile_enabled() -> bool {
    *ADMISSION_PROFILE_ENABLED.get_or_init(|| {
        std::env::var("RPC_ADMISSION_PROFILE")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false)
    })
}

fn update_max_atomic(max: &AtomicU64, value: u64) {
    let mut current = max.load(Ordering::Relaxed);
    while value > current {
        match max.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

struct AdmissionProfileGuard {
    start: Instant,
}

impl AdmissionProfileGuard {
    fn new() -> Option<Self> {
        if !admission_profile_enabled() {
            return None;
        }
        // Sampling decision comes first so an unsampled admission pays no atomics and no
        // clock reads (the per-stage timers below are also gated on the guard's presence).
        let sampled = ADMISSION_SAMPLE_COUNTER.with(|c| {
            let n = c.get().wrapping_add(1);
            c.set(n);
            n % ADMISSION_SAMPLE_EVERY == 0
        });
        if !sampled {
            return None;
        }
        let in_flight = ADMISSION_PROFILE_STATS
            .in_flight
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        update_max_atomic(&ADMISSION_PROFILE_STATS.max_in_flight, in_flight);
        Some(Self {
            start: Instant::now(),
        })
    }

    fn record(
        &self,
        decode: Duration,
        recover: Duration,
        hash_signer: Duration,
        lane_send: Duration,
        direct: bool,
    ) {
        ADMISSION_PROFILE_STATS
            .decode_nanos
            .fetch_add(decode.as_nanos() as u64, Ordering::Relaxed);
        ADMISSION_PROFILE_STATS
            .recover_nanos
            .fetch_add(recover.as_nanos() as u64, Ordering::Relaxed);
        ADMISSION_PROFILE_STATS
            .hash_signer_nanos
            .fetch_add(hash_signer.as_nanos() as u64, Ordering::Relaxed);
        ADMISSION_PROFILE_STATS
            .lane_send_nanos
            .fetch_add(lane_send.as_nanos() as u64, Ordering::Relaxed);
        ADMISSION_PROFILE_STATS
            .total_nanos
            .fetch_add(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if direct {
            ADMISSION_PROFILE_STATS
                .direct
                .fetch_add(1, Ordering::Relaxed);
        }
        let total = ADMISSION_PROFILE_STATS
            .total
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if total % ADMISSION_PROFILE_LOG_EVERY == 0 {
            log_admission_profile(total);
        }
    }
}

impl Drop for AdmissionProfileGuard {
    fn drop(&mut self) {
        ADMISSION_PROFILE_STATS
            .in_flight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Stage timers only tick for sampled admissions — unsampled txs skip the clock reads too.
#[inline]
fn stage_start(profile: &Option<AdmissionProfileGuard>) -> Option<Instant> {
    profile.as_ref().map(|_| Instant::now())
}

#[inline]
fn stage_elapsed(started_at: Option<Instant>) -> Duration {
    started_at.map(|t| t.elapsed()).unwrap_or_default()
}

fn avg_duration(total_nanos: u64, count: u64) -> Duration {
    if count == 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(total_nanos / count)
    }
}

fn log_admission_profile(total: u64) {
    let direct = ADMISSION_PROFILE_STATS.direct.load(Ordering::Relaxed);
    let in_flight = ADMISSION_PROFILE_STATS.in_flight.load(Ordering::Relaxed);
    let max_in_flight = ADMISSION_PROFILE_STATS
        .max_in_flight
        .load(Ordering::Relaxed);
    let avg_decode = avg_duration(
        ADMISSION_PROFILE_STATS.decode_nanos.load(Ordering::Relaxed),
        total,
    );
    let avg_recover = avg_duration(
        ADMISSION_PROFILE_STATS
            .recover_nanos
            .load(Ordering::Relaxed),
        total,
    );
    let avg_hash_signer = avg_duration(
        ADMISSION_PROFILE_STATS
            .hash_signer_nanos
            .load(Ordering::Relaxed),
        total,
    );
    let avg_lane_send = avg_duration(
        ADMISSION_PROFILE_STATS
            .lane_send_nanos
            .load(Ordering::Relaxed),
        direct,
    );
    let avg_total = avg_duration(
        ADMISSION_PROFILE_STATS.total_nanos.load(Ordering::Relaxed),
        total,
    );
    tracing::error!(
        total,
        direct,
        in_flight,
        max_in_flight,
        ?avg_decode,
        ?avg_recover,
        ?avg_hash_signer,
        ?avg_lane_send,
        ?avg_total,
        "rpc admission profile"
    );
}

/// Point-in-time averages of the sampled admission-stage timings (see
/// [`admission_profile_snapshot`]).
#[derive(Debug, Clone, Copy)]
pub struct AdmissionSnapshot {
    /// Sampled admissions recorded (1 per `ADMISSION_SAMPLE_EVERY` per worker thread).
    pub samples: u64,
    /// Sampled admissions that took the direct-lane path (denominator of `avg_lane_send_us`).
    pub direct_samples: u64,
    pub avg_decode_us: f64,
    pub avg_recover_us: f64,
    pub avg_hash_signer_us: f64,
    pub avg_lane_send_us: f64,
    pub avg_total_us: f64,
}

/// Bench/demo-only: averages of the `RPC_ADMISSION_PROFILE` accumulators. `None` when
/// profiling is off or nothing has been sampled yet. Sums and counts are separate relaxed
/// atomics, so a snapshot taken under load can be torn by a sample or two — fine for a
/// live readout, not for accounting.
pub fn admission_profile_snapshot() -> Option<AdmissionSnapshot> {
    if !admission_profile_enabled() {
        return None;
    }
    let samples = ADMISSION_PROFILE_STATS.total.load(Ordering::Relaxed);
    if samples == 0 {
        return None;
    }
    let direct_samples = ADMISSION_PROFILE_STATS.direct.load(Ordering::Relaxed);
    let avg_us = |nanos: &AtomicU64, count: u64| {
        if count == 0 {
            0.0
        } else {
            nanos.load(Ordering::Relaxed) as f64 / count as f64 / 1_000.0
        }
    };
    Some(AdmissionSnapshot {
        samples,
        direct_samples,
        avg_decode_us: avg_us(&ADMISSION_PROFILE_STATS.decode_nanos, samples),
        avg_recover_us: avg_us(&ADMISSION_PROFILE_STATS.recover_nanos, samples),
        avg_hash_signer_us: avg_us(&ADMISSION_PROFILE_STATS.hash_signer_nanos, samples),
        avg_lane_send_us: avg_us(&ADMISSION_PROFILE_STATS.lane_send_nanos, direct_samples),
        avg_total_us: avg_us(&ADMISSION_PROFILE_STATS.total_nanos, samples),
    })
}

/// Bench/demo-only: zero the admission-profile accumulators (e.g. when the demo Start gate
/// releases) so setup-phase traffic doesn't pollute the averages. Fields are cleared one by
/// one while traffic may be in flight — a transient few-sample skew, nothing more.
pub fn admission_profile_reset() {
    ADMISSION_PROFILE_STATS.total.store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS.direct.store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .max_in_flight
        .store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .decode_nanos
        .store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .recover_nanos
        .store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .hash_signer_nanos
        .store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .lane_send_nanos
        .store(0, Ordering::Relaxed);
    ADMISSION_PROFILE_STATS
        .total_nanos
        .store(0, Ordering::Relaxed);
}

/// Test/bench-only: routes RPC-admitted transactions straight into the sequencer's parallel
/// direct-injection lanes, sharded by sender, bypassing the (reth) mempool entirely — no nonce,
/// balance, or tip ordering. Mirrors `DirectTxSource` on the ingestion side and shares its
/// activation flag, so admission only diverts to lanes once `active` is set (the same instant the
/// sequencer flips onto its parallel path). Always `None` in production.
#[derive(Clone)]
pub struct DirectLaneRouter {
    lanes: Vec<tokio::sync::mpsc::Sender<ZkTransaction>>,
    active: Arc<AtomicBool>,
}

impl DirectLaneRouter {
    pub fn new(
        lanes: Vec<tokio::sync::mpsc::Sender<ZkTransaction>>,
        active: Arc<AtomicBool>,
    ) -> Self {
        Self { lanes, active }
    }

    fn is_active(&self) -> bool {
        !self.lanes.is_empty() && self.active.load(Ordering::Relaxed)
    }

    /// Deterministic, sender-stable lane index, so every tx from one account lands in the same lane
    /// (keeping its nonces contiguous in a single FIFO). Address bytes are effectively uniform, so
    /// the low 8 bytes modulo the lane count spread wallets across lanes evenly enough for a load test.
    fn lane_for(&self, signer: &Address) -> usize {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&signer.as_slice()[12..20]);
        (u64::from_le_bytes(buf) % self.lanes.len() as u64) as usize
    }
}

/// Handles transactions received in API
pub struct TxHandler<RpcStorage, Mempool> {
    config: RpcConfig,
    storage: RpcStorage,
    chain_id: u64,
    mempool: Mempool,
    /// Test/bench-only lane router. When present and active, RPC admission shards transactions into
    /// the sequencer's parallel lanes instead of the mempool. `None` in production.
    direct_lanes: Option<DirectLaneRouter>,
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    tx_forwarder: Option<TxForwarder>,
    /// Optional policy client. When set, each incoming tx is simulated
    /// once with the validator wired in (admit + judge inline). Spares
    /// clients a `pending → no receipt` poll loop on a stable deny.
    /// Block-build remains authoritative.
    policy_client: Option<PolicyClient>,
    /// Latest block context constructed by the sequencer. `None` until
    /// the sequencer has built at least one block; in that startup
    /// window we synthesize a pending block context from current state.
    last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2Subpool> TxHandler<RpcStorage, Mempool> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RpcConfig,
        storage: RpcStorage,
        chain_id: u64,
        mempool: Mempool,
        direct_lanes: Option<DirectLaneRouter>,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
        tx_forwarder: Option<TxForwarder>,
        policy_client: Option<PolicyClient>,
        last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
    ) -> Self {
        Self {
            config,
            storage,
            chain_id,
            mempool,
            direct_lanes,
            acceptance_state,
            tx_forwarder,
            policy_client,
            last_constructed_block_context,
        }
    }

    /// Shared prelude of both sync and async send: decode, validate, insert into mempool.
    async fn admit_to_local_mempool(
        &self,
        tx_bytes: &Bytes,
    ) -> Result<B256, EthSendRawTransactionError> {
        let profile = AdmissionProfileGuard::new();
        if let TransactionAcceptanceState::NotAccepting(reasons) = &*self.acceptance_state.borrow()
        {
            return Err(EthSendRawTransactionError::NotAcceptingTransactions(
                reasons.clone(),
            ));
        }

        let decode_started_at = stage_start(&profile);
        let transaction = L2Envelope::decode_2718(&mut tx_bytes.as_ref())
            .map_err(|_| EthSendRawTransactionError::FailedToDecodeSignedTransaction)?;
        let decode_elapsed = stage_elapsed(decode_started_at);
        let recover_started_at = stage_start(&profile);
        let l2_tx: L2Transaction = transaction
            .try_into_recovered()
            .map_err(|_| EthSendRawTransactionError::InvalidTransactionSignature)?;
        let recover_elapsed = stage_elapsed(recover_started_at);
        let hash_signer_started_at = stage_start(&profile);
        let hash = *l2_tx.hash();
        let signer = l2_tx.signer();
        if self.config.l2_signer_blacklist.contains(&signer) {
            return Err(EthSendRawTransactionError::BlacklistedSigner);
        }
        let hash_signer_elapsed = stage_elapsed(hash_signer_started_at);

        // Test/bench-only: with the direct-injection lane router active, shard by sender into the
        // sequencer's parallel lanes and skip the mempool entirely (no nonce/balance/tip ordering).
        // Signature recovery above is still paid, so this stays faithful to the "effective" path.
        if let Some(router) = &self.direct_lanes
            && router.is_active()
        {
            let lane = router.lane_for(&signer);
            let zk_tx: ZkTransaction = l2_tx.into();
            let lane_send_started_at = stage_start(&profile);
            router.lanes[lane]
                .send(zk_tx)
                .await
                .map_err(|_| EthSendRawTransactionError::DirectLaneClosed)?;
            let lane_send_elapsed = stage_elapsed(lane_send_started_at);
            if let Some(profile) = &profile {
                profile.record(
                    decode_elapsed,
                    recover_elapsed,
                    hash_signer_elapsed,
                    lane_send_elapsed,
                    true,
                );
            }
            return Ok(hash);
        }

        if let Some(policy_client) = &self.policy_client {
            // `last_constructed_block_context` is None until the sequencer has
            // prepared its first block. In that startup window, fall back to a
            // synthesized pending block context derived from current state so
            // the policy is still consulted (block-build remains authoritative).
            // Copy the watch ref before the move so the future stays `Send`.
            let last_block_ctx = *self.last_constructed_block_context.borrow();
            let storage = self.storage.clone();
            let chain_id = self.chain_id;
            let zk_tx: ZkTransaction = l2_tx.clone().into();
            let policy_client = policy_client.clone();
            // `spawn_blocking`: the body has blocking I/O and VM execution.
            //
            // TODO: dropping the outer future (RPC client disconnect) does not cancel
            // this task; admit and judge fire to completion. Era stops VM execution on
            // disconnect via a stop token embedded in the tracer and checked during
            // storage operations:
            // https://github.com/matter-labs/zksync-era/blob/main/core/lib/vm_executor/src/oneshot/mod.rs
            // To be worth implementing it would also need to cover `eth_call` and
            // `eth_estimateGas`, which currently run under `#[method(blocking)]`.
            let sim = tokio::task::spawn_blocking(move || {
                let block_context = last_block_ctx
                    .unwrap_or_else(|| build_pending_block_context(&storage, chain_id));
                let storage_view =
                    storage.state_at_block_number_or_latest(block_context.block_number)?;
                let mut policy_session = policy_client.session(AccessType::Write);
                let mut tracer = policy_session.paired_tracer();
                crate::sandbox::execute_with(
                    zk_tx,
                    block_context,
                    storage_view,
                    &mut tracer,
                    &mut policy_session,
                )
            })
            .await
            .map_err(|err| EthSendRawTransactionError::JudgeSimFailed(err.into()))?
            .map_err(EthSendRawTransactionError::JudgeSimFailed)?;
            if matches!(sim, Err(InvalidTransaction::FilteredByValidator)) {
                return Err(EthSendRawTransactionError::PolicyDenied);
            }
            // Other sim errors (nonce, gas, etc.) are handled by the
            // mempool / block-build rejection paths.
        }
        {
            let _guard = MempoolLatencyGuard::new();
            self.mempool.add_l2_transaction(l2_tx).await?;
        }
        if let Some(profile) = &profile {
            profile.record(
                decode_elapsed,
                recover_elapsed,
                hash_signer_elapsed,
                Duration::ZERO,
                false,
            );
        }
        Ok(hash)
    }

    pub async fn send_raw_transaction_impl(
        &self,
        tx_bytes: Bytes,
    ) -> Result<B256, EthSendRawTransactionError> {
        let hash = self.admit_to_local_mempool(&tx_bytes).await?;

        if let Some(tx_forwarder) = self.tx_forwarder.as_ref() {
            // If the handler future is dropped before the forward returns
            // (e.g. client disconnect), `local_cleanup` removes the orphaned
            // local mempool entry on drop. Disarm only on a successful forward.
            let mut local_cleanup = RemoveOnDrop::new(&self.mempool, hash);
            let forwarding_result = {
                let _guard = ForwardingLatencyGuard::new();
                tx_forwarder.forward_raw_transaction(hash, &tx_bytes).await
            };
            // We do not need to wait for pending transaction here, so it's safe to forget about it
            if let Err(err) = forwarding_result {
                tracing::debug!(%err, "transaction forwarding error back to user");
                // `local_cleanup` removes the local mirror as it drops.
                return Err(err.into());
            }
            local_cleanup.disarm();
        }

        Ok(hash)
    }

    pub async fn send_raw_transaction_sync_impl(
        &self,
        bytes: Bytes,
        max_wait_ms: Option<U256>,
    ) -> Result<ZkTransactionReceipt, EthSendRawTransactionSyncError> {
        let timeout_duration = if let Some(timeout_ms) = max_wait_ms {
            match timeout_ms.try_into() {
                Ok(timeout_u64) => {
                    let requested_timeout = Duration::from_millis(timeout_u64);
                    if requested_timeout > SEND_RAW_TRANSACTION_SYNC_MAX_TIMEOUT {
                        // Per EIP-7966 MUST use default timeout if user provided timeout is invalid
                        self.config.send_raw_transaction_sync_timeout
                    } else {
                        requested_timeout
                    }
                }
                Err(_) => {
                    // Per EIP-7966 MUST use default timeout if user provided timeout is invalid
                    self.config.send_raw_transaction_sync_timeout
                }
            }
        } else {
            self.config.send_raw_transaction_sync_timeout
        };

        // Subscribe before submission so the main-node wait below never misses the block.
        let mut block_rx = self.storage.block_subscriptions().subscribe_to_blocks();

        // Admit outside any deadline so a tight user budget can't leave half-admitted state.
        let tx_hash = self.admit_to_local_mempool(&bytes).await?;

        if let Some(forwarder) = self.tx_forwarder.as_ref() {
            // EN path. Main is the source of truth: forward the sync call and
            // return main's verdict (receipt, rejection, or timeout) directly.
            //
            // No client-side timeout wraps this call so the EN cannot disagree
            // with main on the outcome. The trade-off is that main's config
            // default bounds the wait, not the caller's max_wait_ms.
            //
            // If the future is dropped before we learn main's verdict (e.g.,
            // client disconnect), `local_cleanup` removes the orphaned local
            // mempool entry on drop. Disarm only when we explicitly want to
            // keep the local mirror: forward succeeded, or main reported an
            // EIP-7966 timeout (tx still pending on main).
            let mut local_cleanup = RemoveOnDrop::new(&self.mempool, tx_hash);
            let forwarding_result = {
                let _guard = ForwardingLatencyGuard::new();
                forwarder
                    .forward_raw_transaction_sync(tx_hash, &bytes)
                    .await
            };
            match forwarding_result {
                Ok(Some(receipt)) => {
                    local_cleanup.disarm();
                    return Ok(receipt);
                }
                Ok(None) => {
                    local_cleanup.disarm();
                }
                Err(err) => {
                    tracing::debug!(%err, "sync transaction forwarding error back to user");
                    if let Some(RpcError::ErrorResp(payload)) = err.as_rpc_error()
                        && payload.code == EIP_7966_TIMEOUT_CODE
                    {
                        local_cleanup.disarm();
                        return Err(EthSendRawTransactionSyncError::Timeout(timeout_duration));
                    }
                    // Real rejection or transport failure. Let `local_cleanup`
                    // remove the local mirror as it drops.
                    return Err(EthSendRawTransactionError::ForwardError(err).into());
                }
            }
        }

        // Main node: wait for the tx to land in a locally-applied block.
        tokio::time::timeout(timeout_duration, async {
            loop {
                let Ok(block) = block_rx.recv().await else {
                    // Channel closed or is lagging, this shouldn't happen in normal operation
                    tracing::warn!("block subscription closed while waiting for tx receipt");
                    return Err(EthSendRawTransactionSyncError::Timeout(timeout_duration));
                };

                if let Some(stored_tx) = block.transactions.get(&tx_hash) {
                    return Ok(build_api_receipt(
                        tx_hash,
                        stored_tx.receipt.clone(),
                        &stored_tx.tx,
                        &stored_tx.meta,
                    ));
                }

                if let Some(reason) = block.failed_transactions.get(&tx_hash) {
                    return Err(EthSendRawTransactionSyncError::RejectedDuringExecution(
                        reason.clone(),
                    ));
                }
            }
        })
        .await
        .map_err(|_| EthSendRawTransactionSyncError::Timeout(timeout_duration))?
    }
}

/// Error types returned by `eth_sendRawTransaction` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthSendRawTransactionError {
    /// When decoding a signed transaction fails
    #[error("failed to decode signed transaction")]
    FailedToDecodeSignedTransaction,
    /// When the transaction signature is invalid
    #[error("invalid transaction signature")]
    InvalidTransactionSignature,
    /// When the node is not accepting new transactions
    #[error("{}", .0.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("; "))]
    NotAcceptingTransactions(Vec<NotAcceptingReason>),
    /// Errors related to the transaction pool
    #[error(transparent)]
    PoolError(#[from] PoolError),
    /// Error while forwarding transaction
    #[error(transparent)]
    ForwardError(#[from] TxForwardError),
    #[error("Signer is blacklisted")]
    BlacklistedSigner,
    /// Direct-injection lane channel closed (sequencer gone). Test/bench only.
    #[error("direct injection lane closed")]
    DirectLaneClosed,
    /// Policy service rejected the transaction.
    #[error("transaction denied by policy service")]
    PolicyDenied,
    /// Local simulation for the RPC-side judge call failed for an internal
    /// reason (storage error, etc.). Clean tx rejections fall through and
    /// surface via the mempool / block-build paths instead.
    #[error("failed to simulate transaction: {0}")]
    JudgeSimFailed(#[source] anyhow::Error),
}

impl From<&EthSendRawTransactionError> for TxRejectionReason {
    fn from(err: &EthSendRawTransactionError) -> Self {
        match err {
            EthSendRawTransactionError::FailedToDecodeSignedTransaction => Self::DecodeFailed,
            EthSendRawTransactionError::InvalidTransactionSignature => Self::InvalidSignature,
            EthSendRawTransactionError::NotAcceptingTransactions(_) => Self::NotAccepting,
            EthSendRawTransactionError::BlacklistedSigner => Self::BlacklistedSigner,
            EthSendRawTransactionError::DirectLaneClosed => Self::NotAccepting,
            EthSendRawTransactionError::ForwardError(err) => match err {
                TxForwardError::Rpc(RpcError::ErrorResp(_)) => Self::ForwardRejected,
                _ => Self::ForwardTransportError,
            },
            EthSendRawTransactionError::PoolError(pool_err) => Self::from(&pool_err.kind),
            EthSendRawTransactionError::PolicyDenied => Self::PolicyDenied,
            EthSendRawTransactionError::JudgeSimFailed(_) => Self::JudgeSimFailed,
        }
    }
}

impl From<&PoolErrorKind> for TxRejectionReason {
    fn from(kind: &PoolErrorKind) -> Self {
        match kind {
            PoolErrorKind::AlreadyImported => Self::PoolAlreadyImported,
            PoolErrorKind::ReplacementUnderpriced => Self::PoolReplacementUnderpriced,
            PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(_) => Self::PoolFeeCapBelowMinimum,
            PoolErrorKind::SpammerExceededCapacity(_) => Self::PoolSpammerExceededCapacity,
            PoolErrorKind::DiscardedOnInsert => Self::PoolDiscardedOnInsert,
            PoolErrorKind::ExistingConflictingTransactionType(_, _) => Self::PoolConflictingTxType,
            PoolErrorKind::InvalidTransaction(invalid) => Self::from(invalid),
            PoolErrorKind::Other(_) => Self::PoolOther,
        }
    }
}

impl From<&InvalidPoolTransactionError> for TxRejectionReason {
    fn from(err: &InvalidPoolTransactionError) -> Self {
        match err {
            InvalidPoolTransactionError::Consensus(_) => Self::PoolConsensusError,
            InvalidPoolTransactionError::ExceedsGasLimit(_, _) => Self::PoolExceedsGasLimit,
            InvalidPoolTransactionError::MaxTxGasLimitExceeded(_, _) => {
                Self::PoolMaxTxGasLimitExceeded
            }
            InvalidPoolTransactionError::ExceedsFeeCap { .. } => Self::PoolExceedsFeeCap,
            InvalidPoolTransactionError::ExceedsMaxInitCodeSize(_, _) => {
                Self::PoolExceedsMaxInitCodeSize
            }
            InvalidPoolTransactionError::OversizedData { .. } => Self::PoolOversizedData,
            InvalidPoolTransactionError::Underpriced => Self::PoolUnderpriced,
            InvalidPoolTransactionError::Overdraft { .. } => Self::PoolOverdraft,
            InvalidPoolTransactionError::Eip2681 => Self::PoolNonceOverflow,
            InvalidPoolTransactionError::Eip4844(_) => Self::PoolEip4844Error,
            InvalidPoolTransactionError::Eip7702(_) => Self::PoolEip7702Error,
            InvalidPoolTransactionError::Other(_) => Self::PoolOther,
            InvalidPoolTransactionError::IntrinsicGasTooLow => Self::PoolIntrinsicGasTooLow,
            InvalidPoolTransactionError::PriorityFeeBelowMinimum { .. } => {
                Self::PoolPriorityFeeBelowMinimum
            }
        }
    }
}

/// Error types returned by `eth_sendRawTransactionSync` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthSendRawTransactionSyncError {
    /// Regular `eth_sendRawTransaction` errors
    #[error(transparent)]
    Regular(#[from] EthSendRawTransactionError),
    /// Timeout while waiting for transaction receipt.
    #[error("The transaction was added to the mempool but wasn't processed within {0:?}.")]
    Timeout(Duration),
    /// VM rejected the tx during block building. It will not be mined.
    #[error("transaction rejected during execution: {0}")]
    RejectedDuringExecution(InvalidTransaction),
}

/// Records mempool insertion latency on drop, capturing errors and async cancellations.
struct MempoolLatencyGuard(Instant);

impl MempoolLatencyGuard {
    fn new() -> Self {
        Self(Instant::now())
    }
}

impl Drop for MempoolLatencyGuard {
    fn drop(&mut self) {
        TX_SUBMISSION.mempool_latency.observe(self.0.elapsed());
    }
}

/// Records forwarding latency on drop, capturing errors and async cancellations.
struct ForwardingLatencyGuard(Instant);

impl ForwardingLatencyGuard {
    fn new() -> Self {
        Self(Instant::now())
    }
}

impl Drop for ForwardingLatencyGuard {
    fn drop(&mut self) {
        TX_SUBMISSION.forwarding_latency.observe(self.0.elapsed());
    }
}

/// Removes a tx from the local mempool on drop, unless explicitly disarmed.
/// Lets callers reconcile a half-finished forward with the local mirror:
/// if the handler future is dropped (e.g., client disconnect) before the
/// forward returns a verdict, the orphan entry is cleaned up.
struct RemoveOnDrop<'a, Mempool: L2Subpool> {
    mempool: &'a Mempool,
    tx_hash: B256,
    armed: bool,
}

impl<'a, Mempool: L2Subpool> RemoveOnDrop<'a, Mempool> {
    fn new(mempool: &'a Mempool, tx_hash: B256) -> Self {
        Self {
            mempool,
            tx_hash,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<Mempool: L2Subpool> Drop for RemoveOnDrop<'_, Mempool> {
    fn drop(&mut self) {
        if self.armed {
            self.mempool.remove_transactions(vec![self.tx_hash]);
        }
    }
}
