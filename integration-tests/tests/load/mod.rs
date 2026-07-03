use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes, Signature, TxKind, U128, U256, keccak256};
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
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
use zksync_os_integration_tests::contracts::TestERC20;
use zksync_os_integration_tests::{NEXT_TO_L1, TestEnvironment, test_multisetup};
use zksync_os_interface::traits::EncodedTx;
use zksync_os_sequencer::execution::vm_wrapper::VmWrapper;
use zksync_os_server::config::{FeeConfig, StateBackendConfig};
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
    family: &str,
    chain_id: u64,
    index: usize,
    signer: &PrivateKeySigner,
    recipient: Address,
    value: U256,
    count: u64,
) -> anyhow::Result<std::path::PathBuf> {
    // Fold the recipient into the fingerprint so a changed recipient scheme (e.g. the per-wallet
    // recipients of `effective_parallel_tps` vs. the single shared recipient of `effective_tps`)
    // regenerates the corpus instead of silently reusing stale, wrongly-addressed signed txs.
    let recipient_lo = u64::from_be_bytes(recipient.as_slice()[12..20].try_into().unwrap());
    let fp = corpus::fingerprint(&[chain_id, 2 /* rpc scheme version */, recipient_lo]);
    let path = corpus::signer_file(family, index);
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
        .map(|(i, signer)| {
            ensure_rpc_corpus("rpc", chain_id, i, signer, recipient, value, txs_per_file())
        })
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

/// The ERC20 counterpart of [`direct_injection_tps`]: single-lane direct injection of dummy-signed
/// `TestERC20.transfer` calls, isolating the sequencer + execution cost of an ERC20 transfer from RPC
/// / mempool / signing. Same channel-backpressured steady-state measurement — once the pipeline is
/// warm the submit rate equals the sequencer's execution rate.
///
/// A `transfer` runs the full EVM interpreter and rewrites `balances[from]` / `balances[to]` (mapping
/// slots keyed by `keccak`) plus the sender nonce — roughly 5x the per-tx cost of a native transfer —
/// so blocks are capped at `ERC20_MAX_TX`, well below the (much lower than native) ERC20 NativeCycles
/// seal, so a count/cycle seal never consumes-without-executing a tx and opens a permanent nonce gap
/// (the direct channel, unlike the mempool, can't re-serve a dropped tx).
///
/// Set `LOAD_TEST_FLAMEGRAPH=<path>` to profile only the steady-state ERC20 window (in-process pprof) —
/// the cleanest way to get a flamegraph of pure ERC20 execution (no startup / warmup / deploy noise).
///
/// Tunables: `ERC20_MAX_TX` (default 1000), `LOAD_TEST_DURATION_SECS` (60), `LOAD_TEST_WARMUP_SECS` (5).
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn direct_injection_erc20_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let max_tx: usize = env_or("ERC20_MAX_TX", 1000);
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
    // ERC20 transfers cost far more NativeCycles than native transfers, so cap the block well below
    // the NativeCycles seal (tune via `ERC20_MAX_TX`) to avoid early-seal drops -> nonce gaps.
    config.sequencer_config.max_transactions_in_block = max_tx;
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let warmup = Duration::from_secs(env_or("LOAD_TEST_WARMUP_SECS", 5));

    let sender = tester.direct_tx_sender();
    let chain_id = tester.l2_provider.get_chain_id().await?;

    // Deploy the token + mint the single sender (bench_addr(1)) its whole corpus worth of balance
    // (amount 1/tx). Set gas + gas price EXPLICITLY: under `in-memory-storage` the RPC can't estimate
    // gas (no historical block/state to run `eth_estimateGas` against — it returns "block not found").
    let deploy_receipt = TestERC20::deploy_builder(
        tester.l2_provider.clone(),
        U256::ZERO,
        "Load".to_string(),
        "LOAD".to_string(),
    )
    .gas(6_000_000)
    .gas_price(0)
    .send()
    .await?
    .get_receipt()
    .await?;
    let token_addr = deploy_receipt
        .contract_address()
        .expect("deploy receipt missing contract address");
    let token = TestERC20::new(token_addr, tester.l2_provider.clone());
    token
        .mint(bench_addr(1), U256::from(txs_per_file()))
        .gas(300_000)
        .gas_price(0)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    tracing::info!(%token_addr, "deployed TestERC20 + minted sender balance");

    // Signer 0 of the shared ERC20 corpus: bench_addr(1) -> token.transfer(bench_addr(2), 1). Built
    // once (the token address is folded into the fingerprint), reused across runs and shared with
    // `parallel_injection_erc20_tps`'s lane 0 (same signer / recipient / token).
    let signer = bench_addr(1);
    let path = ensure_erc20_corpus(chain_id, 0, token_addr, txs_per_file())?;

    // Switch block production over to the direct channel, then send one native tx through the normal
    // RPC path to flush the producer parked on the (now empty) mempool; from the next block on,
    // production pulls exclusively from the direct channel (fed by the corpus reader below).
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
    // Stream pre-built ERC20 txs from disk instead of constructing them in the hot loop.
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

    // Optional CPU profiling of the steady-state window only (excludes startup/warmup/deploy), when
    // `LOAD_TEST_FLAMEGRAPH=<path>` is set. With RPC + mempool bypassed, this isolates the sequencer +
    // execution pipeline for a pure ERC20 transfer.
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

    let tps = executed as f64 / measured.as_secs_f64();
    let blocks_produced = block_after - block_before;
    tracing::error!(
        max_tx,
        executed,
        ?measured,
        direct_injection_erc20_tps = tps,
        blocks_produced,
        blocks_per_sec = blocks_produced as f64 / measured.as_secs_f64(),
        "direct-injection-erc20 load test complete"
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

/// Corpus family for the dummy-signed ERC20 parallel-injection test.
const ERC20_FAMILY: &str = "erc20";

/// Calldata for `transfer(address,uint256)` (selector `0xa9059cbb`): ABI-encoded recipient + amount.
fn erc20_transfer_calldata(recipient: Address, amount: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 64);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(recipient.as_slice());
    data.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    data
}

/// Like [`build_direct_tx`] but an ERC20 `transfer(recipient, amount)` call to `token` (dummy-signed).
fn build_erc20_tx(
    chain_id: u64,
    nonce: u64,
    token: Address,
    recipient: Address,
    amount: u64,
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
            to: TxKind::Call(token),
            value: U256::ZERO,
            access_list: Default::default(),
            input: erc20_transfer_calldata(recipient, amount).into(),
        }
        .into_signed(signature),
    );
    ZkTransaction::from(Recovered::new_unchecked(envelope, signer))
}

/// Ensure the dummy-signed ERC20 corpus for signer index `i` exists — signer `bench_addr(2i+1)` calls
/// `token.transfer(bench_addr(2i+2), 1)`, nonces `0..count`. The token address is folded into the
/// fingerprint so a redeploy regenerates. Returns the file path.
fn ensure_erc20_corpus(
    chain_id: u64,
    signer_index: usize,
    token: Address,
    count: u64,
) -> anyhow::Result<std::path::PathBuf> {
    let signer = bench_addr(2 * signer_index as u64 + 1);
    let recipient = bench_addr(2 * signer_index as u64 + 2);
    let sig = dummy_signature();
    let token_lo = u64::from_be_bytes(token.as_slice()[12..20].try_into().unwrap());
    let fp = corpus::fingerprint(&[
        chain_id,
        2 * signer_index as u64 + 1,
        2 * signer_index as u64 + 2,
        token_lo,
    ]);
    let path = corpus::signer_file(ERC20_FAMILY, signer_index);
    corpus::ensure_corpus(&path, count, fp, move |n| {
        build_erc20_tx(chain_id, n, token, recipient, 1, signer, sig)
            .inner
            .encoded_2718()
    })?;
    Ok(path)
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

    let senders = tester.direct_tx_lane_senders(k);
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
    // One blocking reader per signer file streams pre-built txs into its own sequencer lane. The
    // signer -> lane mapping is known here, so `produce_parallel` can avoid shared-channel signer
    // rebucketing on every round.
    let pushers: Vec<_> = paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let signer = bench_addr(2 * i as u64 + 1);
            spawn_corpus_pusher(
                path,
                signer,
                senders[i].clone(),
                submitted.clone(),
                stop.clone(),
            )
        })
        .collect();

    // Warm up so the channel fills and the pipeline reaches steady state, then measure the rate.
    tokio::time::sleep(warmup).await;

    if env_or("DIRECT_TX_STOP_PUSHERS_AFTER_WARMUP", false) {
        stop.store(true, Ordering::Relaxed);
    }

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

/// ERC20 counterpart to [`parallel_injection_tps`]: same parallel direct-injection pipeline, but every
/// tx is an ERC20 `transfer(bench_addr(2i+2), 1)` to a shared `TestERC20` (deployed + minted here).
/// The K parallel blocks stay conflict-free because per-lane disjoint addresses give disjoint
/// `balances[from]`/`balances[to]` slots + disjoint nonces; the token contract is a shared READ. This
/// measures the parallel pipeline's throughput under real EVM execution (vs the near-free native
/// transfers of `parallel_injection_tps`).
///
/// `ERC20_MAX_TX` (default 2000) caps the block size: an ERC20 transfer burns far more NativeCycles
/// than a native one, so a block seals on NativeCycles at far fewer txs — keep it below that seal, or
/// `produce_parallel` packs a group past it, the block seals early, and the overflow drops into a
/// permanent nonce gap. Other tunables mirror `parallel_injection_tps`.
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn parallel_injection_erc20_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let k: usize = env_or("PARALLEL_BLOCKS", 2);
    let max_tx: usize = env_or("ERC20_MAX_TX", 2000);
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
    // ERC20 transfers cost far more NativeCycles than native transfers, so cap the block well below
    // the NativeCycles seal (tune via `ERC20_MAX_TX`) to avoid early-seal drops -> nonce gaps.
    config.sequencer_config.max_transactions_in_block = max_tx;
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;
    config.sequencer_config.parallel_blocks = k;
    config.sequencer_config.parallel_block_linger =
        Duration::from_millis(env_or("PARALLEL_BLOCK_LINGER_MS", 5));
    // In-memory base state: the RocksDB read path (an iterator seek per storage read) collapses
    // under K-way concurrent VM execution and dominates exec time.
    config.general_config.state_backend = StateBackendConfig::InMemory;
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let warmup = Duration::from_secs(env_or("LOAD_TEST_WARMUP_SECS", 5));

    let senders = tester.direct_tx_lane_senders(k);
    let chain_id = tester.l2_provider.get_chain_id().await?;

    // Deploy the token + mint each lane's sender its whole corpus worth of balance (amount 1/tx). Set
    // gas + gas price EXPLICITLY: under `in-memory-storage` the RPC can't estimate gas (there's no
    // historical block/state to run `eth_estimateGas` against — it returns "block not found").
    let deploy_receipt = TestERC20::deploy_builder(
        tester.l2_provider.clone(),
        U256::ZERO,
        "Load".to_string(),
        "LOAD".to_string(),
    )
    .gas(6_000_000)
    .gas_price(0)
    .send()
    .await?
    .get_receipt()
    .await?;
    let token_addr = deploy_receipt
        .contract_address()
        .expect("deploy receipt missing contract address");
    let token = TestERC20::new(token_addr, tester.l2_provider.clone());
    let mint_amount = U256::from(txs_per_file());
    let mut mint_txs = Vec::with_capacity(k);
    for i in 0..k {
        mint_txs.push(
            token
                .mint(bench_addr(2 * i as u64 + 1), mint_amount)
                .gas(300_000)
                .gas_price(0)
                .send()
                .await?,
        );
    }
    mint_txs.expect_successful_receipts().await?;
    tracing::info!(k, %token_addr, "deployed TestERC20 + minted sender balances");

    // Ensure the K ERC20 corpora (built once), before activating direct injection.
    let paths: Vec<std::path::PathBuf> = (0..k)
        .map(|i| ensure_erc20_corpus(chain_id, i, token_addr, txs_per_file()))
        .collect::<anyhow::Result<_>>()?;

    // Activate parallel direct injection (same kick dance as `parallel_injection_tps`).
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
    let pushers: Vec<_> = paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let signer = bench_addr(2 * i as u64 + 1);
            spawn_corpus_pusher(
                path,
                signer,
                senders[i].clone(),
                submitted.clone(),
                stop.clone(),
            )
        })
        .collect();

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
        max_tx,
        executed,
        ?measured,
        parallel_injection_erc20_tps = tps,
        blocks_produced,
        blocks_per_sec = blocks_produced as f64 / measured.as_secs_f64(),
        "parallel-injection-erc20 load test complete"
    );

    Ok(())
}

/// Like [`effective_tps`], but drives the sequencer's **parallel** block production from the RPC
/// path. A "simple mempool" — the sharding router `DirectLaneRouter` — stands in for the reth pool:
/// once direct injection is activated, each RPC-admitted tx is sharded by `from` into one of K
/// sequencer lanes (no nonce / balance / tip ordering), and `BlockExecutor` produces K slot-disjoint
/// blocks per round in parallel (see `BlockContextProvider::produce_parallel`). RPC ingestion, real
/// ECDSA recovery, and receipt polling all stay in the path, so this measures the *effective*
/// throughput of the parallel pipeline — the counterpart to [`parallel_injection_tps`], which feeds
/// the lanes directly and bypasses RPC + mempool.
///
/// Correctness rests on the K parallel blocks touching **disjoint accounts** (they all read the same
/// base snapshot and are applied last-writer-wins): routing by `from` keeps every wallet in a single
/// lane, and each wallet sends to its OWN unique recipient (`bench_addr(i)`), so no two lanes write
/// the same account. Fees are 0 so the shared coinbase is never written, and the seal config matches
/// [`direct_injection_tps`] so no tx is dropped into a permanent nonce gap (a lane can't re-serve a
/// dropped tx). `LOAD_TEST_WALLETS` should stay comfortably above `PARALLEL_BLOCKS` so every lane
/// gets at least one wallet — an empty lane stalls the round.
///
/// Submission is **nonce-major**: each round sends one tx per wallet, all at the same nonce, split
/// into JSON-RPC batches of up to `LOAD_TEST_SUBMIT_PIPELINE` txs. A batch's calls are all distinct
/// accounts, so their (concurrent) processing order is irrelevant; and the next nonce round starts
/// only after the current round's sends return, so each wallet's nonce n reaches its lane before its
/// nonce n+1. This preserves per-wallet order WITHOUT relying on the node processing a batch in order
/// (jsonrpsee dispatches batch entries concurrently) — a plain per-wallet pipeline of >1 in-flight
/// txs reorders and gets purged `NonceTooHigh`.
///
/// Tunables: `PARALLEL_BLOCKS` (default 2), `LOAD_TEST_DURATION_SECS` (60), `LOAD_TEST_WALLETS`
/// (128), `LOAD_TEST_CONCURRENCY` (16384), `LOAD_TEST_SUBMIT_PIPELINE` (batch size, default 1).
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn effective_parallel_tps(env: TestEnvironment) -> anyhow::Result<()> {
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
    // Match `direct_injection_tps`'s seal-safety knobs: a count-based seal just under the VM's
    // NativeCycles limit, and a gas limit high enough that no block seals on `GasLimit` (which
    // consumes-without-executing the triggering tx and would open a permanent nonce gap the lane
    // cannot re-serve).
    config.sequencer_config.max_transactions_in_block = 29_200;
    config.sequencer_config.block_gas_limit = 1_000_000_000_000;
    config.sequencer_config.parallel_blocks = k;
    // Batch bursty RPC arrivals into larger blocks: without lingering the producer outruns ingestion
    // and seals near-empty lanes every loop (~4-5 txs/block), and the per-block Merkle `tree_manager`
    // — the slowest consumer — can't keep up, so throughput is block-rate-bound far below the
    // injection path. A few ms of linger lets each round accumulate many more txs per lane, cutting
    // the block rate the tree must sustain. Tunable via `PARALLEL_BLOCK_LINGER_MS` (default 5).
    config.sequencer_config.parallel_block_linger =
        Duration::from_millis(env_or("PARALLEL_BLOCK_LINGER_MS", 5));
    // In-memory state backend, like the injection benches: the RocksDB FullDiffs backend does an
    // iterator seek per server-side state read, which collapses under 128-way concurrent blocks
    // (300-400µs/read observed on a 7995WX — ~25ms of VM-thread time per block).
    config.general_config.state_backend = StateBackendConfig::InMemory;
    // Receipt visibility: the in-memory repository prunes receipts `blocks_to_retain_in_memory`
    // blocks after inclusion (default 512). At bench block rates (1000s of blocks/s) that is a
    // sub-second window — receipt waiters that lag it find nothing, ever. Floor the retention so
    // receipts survive the full confirmation path; an explicit
    // `general_blocks_to_retain_in_memory` env still raises it further.
    config.general_config.blocks_to_retain_in_memory =
        config.general_config.blocks_to_retain_in_memory.max(100_000);
    // Backpressure stays ENABLED (default limits). Even with lingering the tree is the slowest
    // consumer; when it falls behind the node gracefully stops accepting txs. Disabling backpressure
    // instead lets the applier->tree channel overflow and panic ("consumer is catastrophically
    // behind"), crashing the node. The submit loop below treats "not accepting" as transient and
    // backs off, so the measured rate self-regulates to what the full pipeline (tree included) can
    // sustain.
    let tester = env.launch(config).await?;

    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 60));
    let num_wallets: usize = env_or("LOAD_TEST_WALLETS", 128);
    let concurrency: usize = env_or("LOAD_TEST_CONCURRENCY", 16384);
    let submit_pipeline: usize = env_or("LOAD_TEST_SUBMIT_PIPELINE", 1);
    let wait_for_receipts = env_or("LOAD_TEST_WAIT_FOR_RECEIPTS", true);
    let wait_for_final_receipts = !wait_for_receipts && env_or("LOAD_TEST_FINAL_RECEIPTS", false);
    let rpc_urls = tester.l2_rpc_ws_urls();
    assert!(
        num_wallets >= k,
        "LOAD_TEST_WALLETS must be >= PARALLEL_BLOCKS (and ideally well above it)"
    );
    assert!(concurrency > 0, "LOAD_TEST_CONCURRENCY must be > 0");
    assert!(submit_pipeline > 0, "LOAD_TEST_SUBMIT_PIPELINE must be > 0");

    // 10x buffer on gas price so the funding txs never get stuck/evicted in the mempool.
    let gas_price = tester.l2_provider.get_gas_price().await? * 10;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let value = U256::from(1);

    // Deterministic sender wallets (so the pre-signed corpus is reusable across runs) + a plain
    // provider per wallet for RPC concurrency. Each wallet sends to its OWN unique recipient
    // (`bench_addr(i)`), so the per-`from` lane router yields K blocks whose touched account sets
    // ({wallet_i, recipient_i}) are disjoint across lanes.
    let mut signers = Vec::with_capacity(num_wallets);
    let mut recipients = Vec::with_capacity(num_wallets);
    for i in 0..num_wallets {
        let key = keccak256(format!("zksync-os-loadtest-signer-{i}"));
        signers.push(PrivateKeySigner::from_slice(key.as_slice()).expect("valid signing key"));
        recipients.push(bench_addr(i as u64));
    }
    // A pool of `RpcClient`s (one per RPC listener) for issuing cross-wallet JSON-RPC batches over
    // WebSocket — `client.new_batch()` isn't reachable from a built provider — plus a single provider
    // (on the first client) for receipt watching (`get_receipt` shares one block subscription).
    let mut clients = Vec::with_capacity(rpc_urls.len());
    for url in &rpc_urls {
        clients.push(ClientBuilder::default().connect(url.as_str()).await?);
    }
    let receipt_provider =
        DynProvider::new(ProviderBuilder::new().connect_client(clients[0].clone()));
    let root = receipt_provider.root().clone();

    // Each wallet's real-signed corpus (one-time; real ECDSA signing is expensive). A distinct
    // corpus family from `effective_tps` so the two tests never invalidate each other's files; the
    // per-wallet recipient is folded into the fingerprint.
    let paths: Vec<std::path::PathBuf> = signers
        .iter()
        .enumerate()
        .map(|(i, signer)| {
            ensure_rpc_corpus(
                "rpc-parallel",
                chain_id,
                i,
                signer,
                recipients[i],
                value,
                txs_per_file(),
            )
        })
        .collect::<anyhow::Result<_>>()?;

    // Fund every sender from alice (covers the per-tx value; fees are 0). Split at most half of
    // alice's balance evenly so total funding never exceeds what she holds.
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
    tracing::info!(num_wallets, k, %funding, "funded sender wallets");

    // Switch block production onto the parallel direct-injection path. This mirrors the transition
    // in `parallel_injection_tps`: let the producer settle into the serial `produce()` parked on the
    // (now-empty) mempool, activate direct injection, then send a "kick" through the mempool. The
    // in-flight serial call consumes the kick, seals, and only the *next* loop iteration hits the
    // parallel gate — entering `produce_parallel`, parked on the (still-empty) lanes. Crucially the
    // RPC router stays on the mempool path here so the kick actually reaches it; we flip admission
    // over to the lanes only afterwards. (A naive activate-then-submit hangs: the in-flight serial
    // `best_transactions_stream().await` blocks forever on the empty mempool, so the loop never
    // re-checks the parallel gate.)
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
    // From here, RPC admission shards each tx by `from` into one of the K lanes instead of the
    // mempool, and the producer drains those lanes into K parallel blocks per round.
    tester.activate_router();

    let sem = Arc::new(Semaphore::new(concurrency));
    let submitted = Arc::new(AtomicU64::new(0));
    let confirmed = Arc::new(AtomicU64::new(0));
    let final_receipts_confirmed = Arc::new(AtomicU64::new(0));
    let submit_latency_micros = Arc::new(AtomicU64::new(0));
    let latency_micros = Arc::new(AtomicU64::new(0));
    let final_receipt_latency_micros = Arc::new(AtomicU64::new(0));
    // Receipts whose heartbeat watcher missed the inclusion (fell back to direct polling); a
    // large value means the ws newHeads subscription is dropping heads under the block rate.
    let watcher_missed = Arc::new(AtomicU64::new(0));

    // Optional CPU profiling: `LOAD_TEST_FLAMEGRAPH=<path>` samples the whole (in-process node +
    // client) for the run and writes a flamegraph SVG, to attribute where CPU actually goes on the
    // RPC path (e.g. ECDSA signature recovery in ingestion).
    let flamegraph_path = std::env::var("LOAD_TEST_FLAMEGRAPH").ok();
    let profiler_guard = flamegraph_path.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(499)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("failed to start profiler")
    });

    let start = Instant::now();
    let deadline = start + duration;

    // Submit "nonce-major": each round sends one tx PER WALLET, all at the SAME nonce. A batch's
    // calls are then all distinct accounts, so their order within the batch is irrelevant (jsonrpsee
    // dispatches batch entries concurrently) — and we only start the next nonce after the whole
    // round's sends have returned, so every wallet's nonce n reaches its lane before its nonce n+1.
    // This preserves per-wallet order WITHOUT relying on in-order batch processing. Within a round the
    // wallets are split into batches of up to `submit_pipeline` txs, sent concurrently over the
    // client pool (`round-robin`). Receipts are watched by a detached waiter per tx, permit-bounded.
    let mut readers: Vec<corpus::CorpusReader> = paths
        .iter()
        .map(|p| corpus::CorpusReader::open(p))
        .collect::<anyhow::Result<_>>()?;
    let mut receipts = JoinSet::new();
    let mut last_round_hashes: Vec<B256> = Vec::new();
    let mut last_round_sent_at = start;
    // Fully-accepted nonce rounds; used by the failure diagnostics below to tell a stalled
    // wallet (on-chain nonce < rounds submitted => a tx was accepted but never executed) from an
    // executed-but-receipt-invisible tx.
    let mut rounds_submitted: u64 = 0;
    'rounds: while Instant::now() < deadline {
        // One tx from every wallet — all at the current nonce (readers advance in lockstep).
        let mut round: Vec<Vec<u8>> = Vec::with_capacity(num_wallets);
        for reader in &mut readers {
            match reader.next_record()? {
                Some(raw) => round.push(raw),
                None => break 'rounds, // a wallet's corpus is exhausted
            }
        }

        // Send this nonce's txs as chunked JSON-RPC batches, concurrently across the client pool.
        // Throttling ("not currently accepting transactions") is a global gate, so a batch is rejected
        // atomically; resend it after a short backoff. Any other RPC error is a real failure.
        let sent_at = Instant::now();
        let mut batch_tasks = JoinSet::new();
        for (idx, chunk) in round.chunks(submit_pipeline).enumerate() {
            let client = clients[idx % clients.len()].clone();
            let chunk: Vec<Vec<u8>> = chunk.to_vec();
            batch_tasks.spawn(async move {
                loop {
                    let mut batch = client.new_batch();
                    let mut waiters = Vec::with_capacity(chunk.len());
                    for raw in &chunk {
                        waiters.push(batch.add_call::<_, B256>(
                            "eth_sendRawTransaction",
                            &(Bytes::copy_from_slice(raw),),
                        )?);
                    }
                    batch.send().await?;
                    let mut hs = Vec::with_capacity(chunk.len());
                    let mut throttled = 0usize;
                    for w in waiters {
                        match w.await {
                            Ok(h) => hs.push(h),
                            Err(e) if e.to_string().contains("accepting") => throttled += 1,
                            Err(e) => return Err(anyhow::Error::from(e)),
                        }
                    }
                    if throttled > 0 {
                        anyhow::ensure!(
                            hs.is_empty(),
                            "partial batch admission: {} of {} accepted",
                            hs.len(),
                            chunk.len()
                        );
                        if Instant::now() >= deadline {
                            return anyhow::Ok(Vec::new());
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue; // resend the whole chunk
                    }
                    return anyhow::Ok(hs);
                }
            });
        }

        // Barrier: every batch of this nonce must be accepted before we advance to the next nonce.
        let mut hashes = Vec::with_capacity(round.len());
        while let Some(res) = batch_tasks.join_next().await {
            hashes.extend(res??);
        }
        if hashes.is_empty() {
            break; // deadline reached mid-round
        }
        submitted.fetch_add(hashes.len() as u64, Ordering::Relaxed);
        submit_latency_micros.fetch_add(sent_at.elapsed().as_micros() as u64, Ordering::Relaxed);
        if hashes.len() == num_wallets {
            rounds_submitted += 1;
        }

        if wait_for_receipts {
            for hash in hashes {
                let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
                let confirmed = confirmed.clone();
                let latency_micros = latency_micros.clone();
                let watcher_missed = watcher_missed.clone();
                let root = root.clone();
                receipts.spawn(async move {
                    // Fast path: the provider's heartbeat watcher. At bench block rates
                    // (1000s of newHeads/s over one ws subscription) the heartbeat drops
                    // heads, and a watcher whose block was skipped hangs forever — so give
                    // it a short slice and fall back to authoritative direct receipt
                    // polling. Only the missed fraction ever polls.
                    const WATCH_SLICE: Duration = Duration::from_secs(10);
                    let watched = PendingTransactionBuilder::new(root.clone(), hash)
                        .with_timeout(Some(WATCH_SLICE))
                        .get_receipt()
                        .await;
                    let receipt = match watched {
                        Ok(receipt) => receipt,
                        Err(_) => {
                            watcher_missed.fetch_add(1, Ordering::Relaxed);
                            loop {
                                let polled = root
                                    .get_transaction_receipt(hash)
                                    .await
                                    .map_err(|e| {
                                        anyhow::anyhow!("getTransactionReceipt {hash}: {e}")
                                    })?;
                                match polled {
                                    Some(receipt) => break receipt,
                                    None if sent_at.elapsed() > RECEIPT_TIMEOUT => {
                                        anyhow::bail!(
                                            "receipt for tx {hash} not found within timeout (direct poll)"
                                        );
                                    }
                                    None => tokio::time::sleep(Duration::from_millis(500)).await,
                                }
                            }
                        }
                    };
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
        } else if wait_for_final_receipts {
            last_round_hashes = hashes;
            last_round_sent_at = sent_at;
        }
    }

    // In final-receipts mode, confirm the last submitted nonce round landed (proxy for the run).
    if wait_for_final_receipts {
        for hash in last_round_hashes {
            let receipt = PendingTransactionBuilder::new(root.clone(), hash)
                .with_timeout(Some(RECEIPT_TIMEOUT))
                .get_receipt()
                .await?;
            anyhow::ensure!(
                receipt.status(),
                "transaction reverted: {:?}",
                receipt.transaction_hash()
            );
            final_receipts_confirmed.fetch_add(1, Ordering::Relaxed);
            final_receipt_latency_micros.fetch_add(
                last_round_sent_at.elapsed().as_micros() as u64,
                Ordering::Relaxed,
            );
        }
    }
    // Drain receipt waiters, collecting failures instead of aborting on the first: a single lost
    // tx breaks its wallet's whole nonce chain, and the aggregate + per-wallet audit below is far
    // more diagnostic than one opaque timeout.
    let mut receipt_failed = 0u64;
    let mut receipt_errors: Vec<String> = Vec::new();
    while let Some(res) = receipts.join_next().await {
        if let Err(e) = res? {
            receipt_failed += 1;
            if receipt_errors.len() < 8 {
                receipt_errors.push(e.to_string());
            }
        }
    }
    if receipt_failed > 0 {
        // Tell apart "accepted but never executed" (sequencer-side loss: the wallet's on-chain
        // nonce stalls below the rounds submitted) from "executed but receipt not visible"
        // (API-side: nonces all advanced). A stalled wallet's first missing nonce is its current
        // on-chain nonce; the node-side purge WARN log identifies the VM error for that tx.
        let mut stalled = 0usize;
        for (i, signer) in signers.iter().enumerate() {
            let nonce = receipt_provider
                .get_transaction_count(signer.address())
                .await?;
            if nonce < rounds_submitted {
                stalled += 1;
                if stalled <= 16 {
                    tracing::error!(
                        wallet = i,
                        address = %signer.address(),
                        on_chain_nonce = nonce,
                        rounds_submitted,
                        "wallet nonce chain stalled: its tx at nonce `on_chain_nonce` was accepted but never executed"
                    );
                }
            }
        }
        anyhow::bail!(
            "{receipt_failed} receipt waits failed; {stalled} of {num_wallets} wallets have stalled nonce chains (rounds_submitted={rounds_submitted}); first errors: {receipt_errors:#?}"
        );
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

    let elapsed = start.elapsed();
    let submitted = submitted.load(Ordering::Relaxed);
    let confirmed = confirmed.load(Ordering::Relaxed);
    let final_receipts_confirmed = final_receipts_confirmed.load(Ordering::Relaxed);
    let submitted_tps = submitted as f64 / elapsed.as_secs_f64();
    let submitted_window_tps = submitted as f64 / duration.as_secs_f64();
    let effective_tps = confirmed as f64 / elapsed.as_secs_f64();
    let avg_submit_latency = if submitted > 0 {
        Duration::from_micros(submit_latency_micros.load(Ordering::Relaxed) / submitted)
    } else {
        Duration::ZERO
    };
    let avg_latency = if confirmed > 0 {
        Duration::from_micros(latency_micros.load(Ordering::Relaxed) / confirmed)
    } else {
        Duration::ZERO
    };
    let avg_final_receipt_latency = if final_receipts_confirmed > 0 {
        Duration::from_micros(
            final_receipt_latency_micros.load(Ordering::Relaxed) / final_receipts_confirmed,
        )
    } else {
        Duration::ZERO
    };
    tracing::error!(
        k,
        num_wallets,
        concurrency,
        wait_for_receipts,
        wait_for_final_receipts,
        submit_pipeline,
        rpc_listeners = rpc_urls.len(),
        submitted,
        confirmed,
        final_receipts_confirmed,
        watcher_missed = watcher_missed.load(Ordering::Relaxed),
        ?elapsed,
        submitted_parallel_tps = submitted_tps,
        submitted_window_tps,
        effective_parallel_tps = effective_tps,
        ?avg_submit_latency,
        ?avg_latency,
        ?avg_final_receipt_latency,
        "parallel effective load test complete"
    );

    Ok(())
}
