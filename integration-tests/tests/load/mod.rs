use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::{Address, Signature, TxKind, U128, U256, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use zksync_os_integration_tests::assert_traits::{ReceiptAssert, ReceiptsAssert};
use zksync_os_integration_tests::{NEXT_TO_L1, TestEnvironment, test_multisetup};
use zksync_os_interface::traits::EncodedTx;
use zksync_os_sequencer::execution::vm_wrapper::VmWrapper;
use zksync_os_server::config::FeeConfig;
use zksync_os_storage_api::{BlockContext, BlockHashes, ReadStateHistory};
use zksync_os_tx_validators::deployment_filter;
use zksync_os_types::{L2Envelope, ZkEnvelope, ZkTransaction, ZksyncOsEncode};

mod corpus;

/// How long to wait for a single transaction's receipt before giving up.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Fixed gas limit for the load transfers. Generous for a base-token transfer and avoids a
/// per-transaction `eth_estimateGas` round-trip (mirrors `tests/node/mempool.rs`).
const LOAD_GAS_LIMIT: u64 = 100_000;

/// Read an environment variable, falling back to `default` if it is unset or unparseable.
fn env_or<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Default per-signer corpus size (env `LOADTEST_TXS_PER_FILE`). 100M is ample for sustained
/// multi-million-TPS runs without exhausting a signer mid-test. Lower it (and/or the signer count)
/// for `effective_tps`, whose real-signed corpus is far more expensive to generate + store.
fn txs_per_file() -> u64 {
    env_or("LOADTEST_TXS_PER_FILE", 100_000_000u64)
}

/// Corpus family for the dummy-signed direct-injection / VM tests (shared `bench_addr(2i+1)` ->
/// `bench_addr(2i+2)` scheme across `direct_injection_tps`, `parallel_injection_tps`,
/// `parallel_blocks_tps`).
const DIRECT_FAMILY: &str = "direct";

/// Dummy signature for VM-bypass txs: the forward-running VM does not validate EOA signatures and the
/// signer is supplied out of band. Recovering a signer from this would yield garbage, so corpus
/// reconstruction uses the known signer via `Recovered::new_unchecked` instead of recovery.
fn dummy_signature() -> Signature {
    Signature::new(Default::default(), Default::default(), false)
}

/// Reconstruct a `ZkTransaction` from stored RLP envelope bytes + the known signer, WITHOUT ECDSA
/// recovery (cheap — the corpus is dummy-signed and the signer is fixed per file index). The keccak
/// tx hash is then computed lazily by the executor's `.encode()`, off the bottleneck feed path.
fn rebuild_zk_tx(rlp: &[u8], signer: Address) -> ZkTransaction {
    let envelope = ZkEnvelope::decode_2718(&mut &rlp[..]).expect("decode_2718 corpus tx");
    let tx = ZkTransaction {
        inner: Recovered::new_unchecked(envelope, signer),
    };
    // Force + cache the keccak tx hash here, on the (parallel) reader thread, so the executor's
    // `.encode()` reads it from the oracle for free — preserving the tx-hash-from-oracle optimization
    // and parallelizing the keccak across the K readers instead of serializing it in the executor.
    let _ = tx.hash();
    tx
}

/// Ensure the dummy-signed corpus for signer index `i` exists — signer `bench_addr(2i+1)` ->
/// recipient `bench_addr(2i+2)`, nonces `0..count`, for `chain_id` — generating it once if needed.
/// Returns the file path.
fn ensure_direct_corpus(
    chain_id: u64,
    signer_index: usize,
    count: u64,
) -> anyhow::Result<std::path::PathBuf> {
    let signer = bench_addr(2 * signer_index as u64 + 1);
    let recipient = bench_addr(2 * signer_index as u64 + 2);
    let sig = dummy_signature();
    let fp = corpus::fingerprint(&[
        chain_id,
        2 * signer_index as u64 + 1,
        2 * signer_index as u64 + 2,
    ]);
    let path = corpus::signer_file(DIRECT_FAMILY, signer_index);
    corpus::ensure_corpus(&path, count, fp, |n| {
        build_direct_tx(chain_id, n, recipient, signer, sig)
            .inner
            .encoded_2718()
    })?;
    Ok(path)
}

/// Spawn a blocking reader that streams signer `signer`'s dummy corpus (`path`) into `sender` as
/// reconstructed `ZkTransaction`s until `stop` is set, the channel closes, or the file is exhausted.
/// Runs on the blocking pool (file reads + `blocking_send` backpressure); counts each tx in
/// `submitted`. Reconstruction skips ECDSA recovery and the keccak tx hash, so the feed is cheap.
fn spawn_corpus_pusher(
    path: std::path::PathBuf,
    signer: Address,
    sender: tokio::sync::mpsc::Sender<ZkTransaction>,
    submitted: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::task::spawn_blocking(move || {
        let mut reader = corpus::CorpusReader::open(&path)?;
        while !stop.load(Ordering::Relaxed) {
            let Some(rlp) = reader.next_record()? else {
                tracing::warn!(
                    path = %path.display(),
                    "corpus exhausted; reader stopping (raise LOADTEST_TXS_PER_FILE)"
                );
                break;
            };
            let tx = rebuild_zk_tx(&rlp, signer);
            if sender.blocking_send(tx).is_err() {
                break; // sequencer dropped the channel
            }
            submitted.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })
}

/// Ensure the REAL-signed RPC corpus for wallet `index` exists: `count` EIP-1559 transfers to
/// `recipient` (value `value`, fee 0, nonces `0..count`), signed with `signer`'s key and EIP-2718
/// encoded for `eth_sendRawTransaction`. Generated once — real ECDSA signing (~tens of µs each), so
/// this is by far the most expensive corpus to build; keep `count` and the wallet count modest.
fn ensure_rpc_corpus(
    chain_id: u64,
    index: usize,
    signer: &PrivateKeySigner,
    recipient: Address,
    value: U256,
    count: u64,
) -> anyhow::Result<std::path::PathBuf> {
    let fp = corpus::fingerprint(&[chain_id, 1 /* rpc scheme version */]);
    let path = corpus::signer_file("rpc", index);
    let signer = signer.clone();
    corpus::ensure_corpus(&path, count, fp, move |n| {
        let tx = TxEip1559 {
            chain_id,
            nonce: n,
            gas_limit: LOAD_GAS_LIMIT,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(recipient),
            value,
            access_list: Default::default(),
            input: Default::default(),
        };
        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("sign corpus tx");
        tx.into_signed(sig).encoded_2718()
    })?;
    Ok(path)
}

/// Drives sustained transaction load for a configurable duration and reports the effective TPS
/// (transactions confirmed per second).
///
/// The generator spreads load across several freshly-funded wallets so submission is not bottle-
/// necked on a single account's strict nonce ordering. Each wallet runs a submit loop bounded by a
/// shared in-flight semaphore: every accepted transaction spawns a detached receipt waiter that
/// releases its permit on confirmation, which both bounds memory and applies backpressure.
///
/// Tunable via environment variables (defaults keep it CI-friendly):
/// - `LOAD_TEST_DURATION_SECS` (default 60): length of the timed submission window.
/// - `LOAD_TEST_WALLETS` (default 64): number of parallel sender wallets.
/// - `LOAD_TEST_CONCURRENCY` (default 8192): global cap on in-flight (sent-but-unconfirmed) txs.
///
/// This is a measurement, not a threshold gate: it logs the result and only fails if a transaction
/// errors or reverts.
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn effective_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.prover_input_generator_config.enable_input_generation = false;
    config.fee_config = FeeConfig {
        base_fee_override: Some(U128::from(0)),
        pubdata_price_override: Some(U128::from(0)),
        ..Default::default()
    };
    config.mempool_config.minimal_protocol_basefee = 0;
    config.mempool_config.max_account_slots = 8192;
    config.sequencer_config.revm_consistency_checker_enabled = false;
    config.batcher_config.enabled = false;
    config.sequencer_config.block_pubdata_limit_bytes = u64::MAX;
    config.sequencer_config.max_transactions_in_block = 10000;
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let num_wallets: usize = env_or("LOAD_TEST_WALLETS", 128);
    let concurrency: usize = env_or("LOAD_TEST_CONCURRENCY", 16384);
    assert!(num_wallets > 0, "LOAD_TEST_WALLETS must be > 0");
    assert!(concurrency > 0, "LOAD_TEST_CONCURRENCY must be > 0");

    // 10x buffer on gas price so txs never get stuck/evicted in the mempool mid-run.
    let gas_price = tester.l2_provider.get_gas_price().await? * 10;
    // Single fixed recipient so per-tx gas stays predictable and we don't churn a new account per
    // transaction.
    let recipient = Address::repeat_byte(0x42);

    let chain_id = tester.l2_provider.get_chain_id().await?;
    let value = U256::from(1);

    // 1. Deterministic sender wallets (so the pre-signed corpus is reusable across runs) + a plain
    //    provider per wallet for RPC concurrency (no wallet attached — we submit pre-signed raw bytes).
    let mut signers = Vec::with_capacity(num_wallets);
    let mut providers = Vec::with_capacity(num_wallets);
    for i in 0..num_wallets {
        let key = keccak256(format!("zksync-os-loadtest-signer-{i}"));
        let signer = PrivateKeySigner::from_slice(key.as_slice()).expect("valid signing key");
        signers.push(signer);
        let provider = ProviderBuilder::new().connect(tester.l2_rpc_url()).await?;
        providers.push(DynProvider::new(provider));
    }

    // Ensure each wallet's real-signed corpus exists (one-time; real ECDSA signing is expensive — at
    // 100M/wallet x many wallets this is very large + slow, so lower LOAD_TEST_WALLETS and/or
    // LOADTEST_TXS_PER_FILE for this test).
    let paths: Vec<std::path::PathBuf> = signers
        .iter()
        .enumerate()
        .map(|(i, signer)| ensure_rpc_corpus(chain_id, i, signer, recipient, value, txs_per_file()))
        .collect::<anyhow::Result<_>>()?;

    // 2. Fund every sender from alice (covers the per-tx value; fees are 0). Split at most half of
    //    alice's balance evenly so total funding never exceeds what she holds.
    let alice = tester.l2_wallet.default_signer().address();
    let alice_balance = tester.l2_provider.get_balance(alice).await?;
    let funding = alice_balance / U256::from((num_wallets * 2) as u64);
    let mut funding_txs = Vec::with_capacity(num_wallets);
    for signer in &signers {
        let tx = TransactionRequest::default()
            .with_to(signer.address())
            .with_value(funding)
            .with_gas_price(gas_price)
            .with_gas_limit(LOAD_GAS_LIMIT);
        funding_txs.push(tester.l2_provider.send_transaction(tx).await?);
    }
    funding_txs.expect_successful_receipts().await?;
    tracing::info!(num_wallets, %funding, "funded sender wallets");

    // No separate priming step: each wallet's first corpus tx (nonce 0) warms it up + creates the
    // recipient account; the warmup absorbed in the timed window's tail.

    // 4. Timed load: one submitter task per wallet, all sharing the in-flight semaphore + counters.
    let sem = Arc::new(Semaphore::new(concurrency));
    let submitted = Arc::new(AtomicU64::new(0));
    let confirmed = Arc::new(AtomicU64::new(0));
    let latency_micros = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let deadline = start + duration;

    // Optional CPU profiling: when `LOAD_TEST_FLAMEGRAPH=<path>` is set, sample the whole process
    // (incl. the VM `spawn_blocking` thread) for the duration of the load and write a flamegraph SVG.
    let flamegraph_path = std::env::var("LOAD_TEST_FLAMEGRAPH").ok();
    let profiler_guard = flamegraph_path.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(499)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("failed to start profiler")
    });

    let mut submitters = Vec::with_capacity(num_wallets);
    for (provider, path) in providers.into_iter().zip(paths) {
        let sem = sem.clone();
        let submitted = submitted.clone();
        let confirmed = confirmed.clone();
        let latency_micros = latency_micros.clone();
        submitters.push(tokio::spawn(async move {
            // Stream pre-signed raw txs from this wallet's corpus and submit via
            // eth_sendRawTransaction (no client-side signing in the hot loop).
            let mut reader = corpus::CorpusReader::open(&path)?;
            let mut receipts = JoinSet::new();
            while Instant::now() < deadline {
                // Acquiring a permit blocks once `concurrency` txs are in flight — backpressure.
                let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
                let Some(raw) = reader.next_record()? else {
                    break; // corpus exhausted
                };
                let sent_at = Instant::now();
                let pending = provider.send_raw_transaction(&raw).await?;
                submitted.fetch_add(1, Ordering::Relaxed);

                let confirmed = confirmed.clone();
                let latency_micros = latency_micros.clone();
                receipts.spawn(async move {
                    let receipt = pending
                        .with_timeout(Some(RECEIPT_TIMEOUT))
                        .get_receipt()
                        .await?;
                    // Release the in-flight slot only once the tx is confirmed.
                    drop(permit);
                    anyhow::ensure!(
                        receipt.status(),
                        "transaction reverted: {:?}",
                        receipt.transaction_hash()
                    );
                    confirmed.fetch_add(1, Ordering::Relaxed);
                    latency_micros
                        .fetch_add(sent_at.elapsed().as_micros() as u64, Ordering::Relaxed);
                    anyhow::Ok(())
                });
            }
            // Drain in-flight receipts and surface any failure.
            while let Some(res) = receipts.join_next().await {
                res??;
            }
            anyhow::Ok(())
        }));
    }
    for submitter in submitters {
        submitter.await??;
    }

    // Render the flamegraph (if profiling was enabled) before computing the summary.
    if let (Some(guard), Some(path)) = (profiler_guard, flamegraph_path.as_ref()) {
        match guard.report().build() {
            Ok(report) => {
                let file = std::fs::File::create(path)?;
                report.flamegraph(file)?;
                tracing::info!(path, "wrote flamegraph");
            }
            Err(err) => tracing::warn!(%err, "failed to build profiler report"),
        }
    }

    // 5. Report. Effective TPS divides confirmed txs by the full elapsed time (submission window +
    //    the short drain tail), which is the honest sustained throughput.
    let elapsed = start.elapsed();
    let submitted = submitted.load(Ordering::Relaxed);
    let confirmed = confirmed.load(Ordering::Relaxed);
    let effective_tps = confirmed as f64 / elapsed.as_secs_f64();
    let avg_latency = if confirmed > 0 {
        Duration::from_micros(latency_micros.load(Ordering::Relaxed) / confirmed)
    } else {
        Duration::ZERO
    };
    tracing::info!(
        num_wallets,
        concurrency,
        submitted,
        confirmed,
        ?elapsed,
        effective_tps,
        ?avg_latency,
        "load test complete"
    );

    Ok(())
}

/// Like [`effective_tps`], but bypasses the RPC and mempool layers entirely: transactions are
/// streamed straight into block production through the sequencer's direct tx channel (activated via
/// [`Tester::activate_direct_injection`]). This isolates the cost of the sequencer + execution
/// pipeline from RPC ingestion, signature recovery, and receipt polling — which the flamegraph
/// showed dominate the RPC-based path.
///
/// Submission is backpressured by the channel, so once the pipeline reaches steady state the submit
/// rate equals the sequencer's execution rate. We warm up, then measure that rate.
///
/// Tunable: `LOAD_TEST_DURATION_SECS` (default 60), `LOAD_TEST_WARMUP_SECS` (default 5).
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn direct_injection_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.prover_input_generator_config.enable_input_generation = false;
    config.fee_config = FeeConfig {
        base_fee_override: Some(U128::from(0)),
        pubdata_price_override: Some(U128::from(0)),
        ..Default::default()
    };
    config.mempool_config.minimal_protocol_basefee = 0;
    config.sequencer_config.revm_consistency_checker_enabled = false;
    config.batcher_config.enabled = false;
    config.sequencer_config.block_pubdata_limit_bytes = u64::MAX;
    // Blocks are sealed because of NativeCycles on 29205 transactions, we set limit just below that
    config.sequencer_config.max_transactions_in_block = 29_200;
    // Raise the gas limit so blocks never seal on `GasLimit`. A limit-based seal consumes the
    // triggering tx from the stream without executing it; the mempool re-serves such a tx, but the
    // direct channel would drop it and open a permanent nonce gap. With this, blocks seal only on
    // the deadline or the tx-count limit, neither of which drops a tx. 1e12 is far above what
    // `max_transactions_in_block` txs need, yet well under the VM's `MAX_BLOCK_GAS_LIMIT`
    // (`u64::MAX / 256`), which `u64::MAX` would exceed.
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let warmup = Duration::from_secs(env_or("LOAD_TEST_WARMUP_SECS", 5));

    let sender = tester.direct_tx_sender();
    let chain_id = tester.l2_provider.get_chain_id().await?;

    // Signer 0 of the shared dummy corpus: bench_addr(1) -> bench_addr(2). Built once, reused.
    let signer = bench_addr(1);
    let path = ensure_direct_corpus(chain_id, 0, txs_per_file())?;

    // Switch block production over to the direct channel, then send one tx through the normal RPC
    // path to flush the producer parked on the (now empty) mempool; from the next block on, production
    // pulls exclusively from the direct channel (fed by the corpus reader below).
    tester.activate_direct_injection();
    let kick = TransactionRequest::default()
        .with_to(bench_addr(2))
        .with_value(U256::ZERO)
        .with_gas_price(0)
        .with_gas_limit(LOAD_GAS_LIMIT);
    tester
        .l2_provider
        .send_transaction(kick)
        .await?
        .expect_successful_receipt()
        .await?;

    let submitted = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // Stream pre-built txs from disk instead of constructing them in the hot loop.
    let pusher = spawn_corpus_pusher(
        path,
        signer,
        sender.clone(),
        submitted.clone(),
        stop.clone(),
    );

    // Warm up so the channel fills and the pipeline reaches steady state, then measure the
    // steady-state consumption rate.
    tokio::time::sleep(warmup).await;

    // Optional CPU profiling of the steady-state window only (excludes startup/warmup), when
    // `LOAD_TEST_FLAMEGRAPH=<path>` is set. With RPC + mempool bypassed, this isolates the
    // sequencer + execution pipeline.
    let flamegraph_path = std::env::var("LOAD_TEST_FLAMEGRAPH").ok();
    let profiler_guard = flamegraph_path.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(499)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("failed to start profiler")
    });

    let submitted_before = submitted.load(Ordering::Relaxed);
    let block_before = tester.l2_provider.get_block_number().await?;
    let measure_start = Instant::now();

    tokio::time::sleep(duration).await;

    let measured = measure_start.elapsed();
    let executed = submitted.load(Ordering::Relaxed) - submitted_before;
    let block_after = tester.l2_provider.get_block_number().await?;
    stop.store(true, Ordering::Relaxed);
    let _ = pusher.await;

    if let (Some(guard), Some(path)) = (profiler_guard, flamegraph_path.as_ref()) {
        match guard.report().build() {
            Ok(report) => {
                let file = std::fs::File::create(path)?;
                report.flamegraph(file)?;
                tracing::info!(path, "wrote flamegraph");
            }
            Err(err) => tracing::warn!(%err, "failed to build profiler report"),
        }
    }

    let direct_injection_tps = executed as f64 / measured.as_secs_f64();
    tracing::info!(
        executed,
        ?measured,
        direct_injection_tps,
        blocks_produced = block_after - block_before,
        "direct-injection load test complete"
    );

    Ok(())
}

/// Build a `ZkTransaction` directly with a known signer and a dummy signature (no ECDSA), as cheaply
/// as possible — used to feed the sequencer's direct tx channel without RPC / mempool / signing.
fn build_direct_tx(
    chain_id: u64,
    nonce: u64,
    recipient: Address,
    signer: Address,
    signature: Signature,
) -> ZkTransaction {
    let envelope = L2Envelope::from(
        TxEip1559 {
            chain_id,
            nonce,
            gas_limit: LOAD_GAS_LIMIT,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(recipient),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Default::default(),
        }
        .into_signed(signature),
    );
    ZkTransaction::from(Recovered::new_unchecked(envelope, signer))
}

/// Deterministic, distinct bench address from an index. The `0xBE` prefix keeps these clear of system
/// / funded accounts so different indices never share a storage slot.
fn bench_addr(n: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0xBE;
    bytes[12..20].copy_from_slice(&n.to_be_bytes());
    Address::from(bytes)
}

/// Proves the FAFO-style thesis: zksync-os's serial `run_block` can be run on K threads over
/// **disjoint** state to multiply execution throughput, without touching the VM or the prover.
///
/// It bypasses the whole sequencer pipeline — it grabs the node's (shared) post-upgrade state handle
/// and drives K [`VmWrapper`] instances concurrently, each executing one block of `M` native
/// transfers from a distinct sender to a distinct recipient (so the K blocks touch disjoint storage
/// slots and are conflict-free). Each `VmWrapper` runs `run_block` on its own `spawn_blocking`
/// thread, so K of them = K parallel `run_block` calls. We sweep K and report aggregate TPS +
/// speedup over the K=1 serial baseline.
///
/// Tunable: `PARALLEL_BENCH_M` (transfers per block, default 29_200).
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn parallel_blocks_tps(env: TestEnvironment) -> anyhow::Result<()> {
    // Same bench knobs as `direct_injection_tps`: gas/fees 0, no prover input, no batcher, huge
    // limits, count-based seal. (Fees 0 means senders need no balance.)
    let mut config = env.default_config().await?;
    config.prover_input_generator_config.enable_input_generation = false;
    config.fee_config = FeeConfig {
        base_fee_override: Some(U128::from(0)),
        pubdata_price_override: Some(U128::from(0)),
        ..Default::default()
    };
    config.mempool_config.minimal_protocol_basefee = 0;
    config.sequencer_config.revm_consistency_checker_enabled = false;
    config.batcher_config.enabled = false;
    config.sequencer_config.block_pubdata_limit_bytes = u64::MAX;
    config.sequencer_config.max_transactions_in_block = 29_200;
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;

    let tester = env.launch(config).await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    // Shared, live state handle. After startup (upgrade + initial deposit) it reflects post-upgrade
    // V6 state. The node now idles (we send no txs), so the base snapshot stays stable.
    let state = tester.state();
    let base = *state.block_range_available().end();
    tracing::info!(
        base,
        chain_id,
        "parallel-blocks bench: base block + chain id"
    );

    let m: usize = env_or("PARALLEL_BENCH_M", 29_200);

    // V6 block context for the bench blocks (all K execute at base+1 against the base snapshot).
    // `block_hashes` is unread for native transfers (no BLOCKHASH); `native_price` mirrors an observed
    // produced block so native-resource accounting behaves.
    let ctx = BlockContext {
        chain_id,
        block_number: base + 1,
        block_hashes: BlockHashes([U256::ZERO; 256]),
        timestamp: 4_000_000_000,
        eip1559_basefee: U256::ZERO,
        pubdata_price: U256::ZERO,
        native_price: U256::from(974_992u64),
        coinbase: Address::ZERO,
        gas_limit: 1_000_000_000_000,
        pubdata_limit: u64::MAX,
        mix_hash: U256::ZERO,
        execution_version: 6,
        blob_fee: U256::ONE,
    };

    let mut baseline_tps = 0.0;
    for k in [1usize, 2, 4, 8, 16] {
        // Load K disjoint groups of M transfers from the per-signer corpus (untimed). Group i: signer
        // bench_addr(2i+1) -> recipient bench_addr(2i+2), nonces 0..M. Each tx is reconstructed from
        // its stored RLP and `.encode()`d for the VM (the keccak hash is computed here, off the timed
        // region). Only M records/signer are needed, so this corpus is sized to M (not the sustained
        // tests' 100M).
        let groups: Vec<Vec<EncodedTx>> = (0..k)
            .map(|i| -> anyhow::Result<Vec<EncodedTx>> {
                let signer = bench_addr(2 * i as u64 + 1);
                let path = ensure_direct_corpus(chain_id, i, m as u64)?;
                let mut reader = corpus::CorpusReader::open(&path)?;
                let mut group = Vec::with_capacity(m);
                for _ in 0..m {
                    let rlp = reader
                        .next_record()?
                        .expect("corpus must contain at least M records");
                    group.push(rebuild_zk_tx(&rlp, signer).encode());
                }
                Ok(group)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let view = state.state_view_at(base).expect("state_view_at(base)");

        // Timed parallel region: K VmWrappers, each its own `run_block` thread.
        let start = Instant::now();
        let mut set = JoinSet::new();
        for txs in groups {
            let view = view.clone();
            let ctx = ctx.clone();
            set.spawn(async move {
                let tracer = deployment_filter::Tracer::new(
                    Arc::new(AtomicBool::new(false)),
                    deployment_filter::Config::Unrestricted,
                );
                let validator = deployment_filter::Validator::new(Arc::new(AtomicBool::new(false)));
                let mut vm =
                    VmWrapper::new(ctx, view, tracer, validator, Arc::new(AtomicU64::new(0)));
                let n = txs.len();
                for tx in txs {
                    vm.submit_tx(tx).await.expect("submit_tx");
                }
                let mut ok = 0usize;
                for _ in 0..n {
                    match vm.next_result().await.expect("next_result") {
                        Ok(_) => ok += 1,
                        Err(e) => panic!("parallel-bench transfer failed: {e:?}"),
                    }
                }
                let out = vm.seal_block().await.expect("seal_block");
                (ok, out)
            });
        }
        let mut outputs = Vec::new();
        while let Some(res) = set.join_next().await {
            outputs.push(res.expect("join VmWrapper task"));
        }
        let elapsed = start.elapsed();

        // Correctness: every transfer executed, and the K blocks are slot-disjoint (the thesis
        // precondition — proves we actually ran conflict-free work in parallel).
        for (ok, _) in &outputs {
            assert_eq!(*ok, m, "every transfer must execute successfully");
        }
        let mut keys = std::collections::HashSet::new();
        for (_, out) in &outputs {
            for w in out.as_ref().storage_writes.iter() {
                assert!(
                    keys.insert(w.key),
                    "slot {:?} written by two parallel blocks — not disjoint",
                    w.key
                );
            }
        }

        let total = (k * m) as f64;
        let tps = total / elapsed.as_secs_f64();
        if k == 1 {
            baseline_tps = tps;
        }
        tracing::error!(
            k,
            txs_per_block = m,
            total_txs = k * m,
            ?elapsed,
            tps,
            speedup = tps / baseline_tps,
            "parallel-blocks bench"
        );
    }
    Ok(())
}

/// End-to-end counterpart to [`parallel_blocks_tps`], but through the **real sequencer pipeline**:
/// sets `parallel_blocks = K` so `BlockExecutor` executes K slot-disjoint blocks per round in parallel
/// (see `BlockContextProvider::produce_parallel`), then flushes them through canonizer -> applier ->
/// tree. The pusher feeds K distinct senders -> K distinct recipients (round-robin) so the executor's
/// sender-bucketing yields K conflict-free groups. Unlike the standalone harness, this measures where
/// the *sequential downstream tail* (applier) caps the full-pipeline speedup.
///
/// Run once per K: `PARALLEL_BLOCKS=2` (default), `=4`, ... K=1 reproduces `direct_injection_tps`.
/// Tunables: `PARALLEL_BLOCKS` (default 2), `LOAD_TEST_DURATION_SECS` (60), `LOAD_TEST_WARMUP_SECS` (5).
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn parallel_injection_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let k: usize = env_or("PARALLEL_BLOCKS", 2);
    let mut config = env.default_config().await?;
    config.prover_input_generator_config.enable_input_generation = false;
    config.fee_config = FeeConfig {
        base_fee_override: Some(U128::from(0)),
        pubdata_price_override: Some(U128::from(0)),
        ..Default::default()
    };
    config.mempool_config.minimal_protocol_basefee = 0;
    config.sequencer_config.revm_consistency_checker_enabled = false;
    config.batcher_config.enabled = false;
    config.sequencer_config.block_pubdata_limit_bytes = u64::MAX;
    config.sequencer_config.max_transactions_in_block = 29_200;
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;
    config.sequencer_config.parallel_blocks = k;
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let warmup = Duration::from_secs(env_or("LOAD_TEST_WARMUP_SECS", 5));

    let sender = tester.direct_tx_sender();
    let chain_id = tester.l2_provider.get_chain_id().await?;

    // Ensure the K signer corpora exist (signer i = bench_addr(2i+1) -> bench_addr(2i+2)), built once
    // and reused. Do this BEFORE activating direct injection so the producer stays parked on the
    // mempool during any (one-time) generation rather than spinning on an empty direct channel.
    let paths: Vec<std::path::PathBuf> = (0..k)
        .map(|i| ensure_direct_corpus(chain_id, i, txs_per_file()))
        .collect::<anyhow::Result<_>>()?;

    // Let the producer settle into the serial `produce()` parked on the (empty) mempool before we
    // activate. Then the kick below is processed by that in-flight serial call; only the *next* loop
    // iteration hits the parallel gate. Activating while the producer is already in `produce_parallel`
    // would strand the (mempool-only) kick and hang on its receipt.
    tokio::time::sleep(Duration::from_secs(1)).await;
    tester.activate_direct_injection();
    let kick = TransactionRequest::default()
        .with_to(Address::repeat_byte(0x42))
        .with_value(U256::ZERO)
        .with_gas_price(0)
        .with_gas_limit(LOAD_GAS_LIMIT);
    tester
        .l2_provider
        .send_transaction(kick)
        .await?
        .expect_successful_receipt()
        .await?;

    let submitted = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // One blocking reader per signer file streams pre-built txs into the shared channel; the sequencer
    // buckets by signer into K conflict-free groups. Reading from disk (no build, no keccak) is cheap,
    // so K parallel readers feed far faster than the old single build-loop pusher (~1.24M ceiling).
    let pushers: Vec<_> = paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let signer = bench_addr(2 * i as u64 + 1);
            spawn_corpus_pusher(
                path,
                signer,
                sender.clone(),
                submitted.clone(),
                stop.clone(),
            )
        })
        .collect();

    // Warm up so the channel fills and the pipeline reaches steady state, then measure the rate.
    tokio::time::sleep(warmup).await;

    let submitted_before = submitted.load(Ordering::Relaxed);
    let block_before = tester.l2_provider.get_block_number().await?;
    let measure_start = Instant::now();

    tokio::time::sleep(duration).await;

    let measured = measure_start.elapsed();
    let executed = submitted.load(Ordering::Relaxed) - submitted_before;
    let block_after = tester.l2_provider.get_block_number().await?;
    stop.store(true, Ordering::Relaxed);
    for pusher in pushers {
        let _ = pusher.await;
    }

    let tps = executed as f64 / measured.as_secs_f64();
    let blocks_produced = block_after - block_before;
    tracing::error!(
        k,
        executed,
        ?measured,
        parallel_injection_tps = tps,
        blocks_produced,
        blocks_per_sec = blocks_produced as f64 / measured.as_secs_f64(),
        "parallel-injection load test complete"
    );

    Ok(())
}
