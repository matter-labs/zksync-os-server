use alloy::network::{EthereumWallet, ReceiptResponse, TransactionBuilder, TxSigner};
use alloy::primitives::{Address, U128, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use zksync_os_integration_tests::assert_traits::ReceiptsAssert;
use zksync_os_integration_tests::{NEXT_TO_L1, TestEnvironment, test_multisetup};
use zksync_os_server::config::FeeConfig;

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

    // 1. Derive sender wallets and build one provider per wallet (each gets its own cached nonce
    //    manager, so nonces auto-increment correctly under concurrency).
    let mut wallets = Vec::with_capacity(num_wallets);
    let mut providers = Vec::with_capacity(num_wallets);
    for _ in 0..num_wallets {
        let signer = PrivateKeySigner::random();
        wallets.push(signer.address());
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(signer))
            .connect(tester.l2_rpc_url())
            .await?;
        providers.push(DynProvider::new(provider));
    }

    // 2. Fund every sender from alice in one concurrent batch. Split (at most) half of alice's
    //    balance evenly so total funding never exceeds what she holds, regardless of wallet count.
    let alice = tester.l2_wallet.default_signer().address();
    let alice_balance = tester.l2_provider.get_balance(alice).await?;
    let funding = alice_balance / U256::from((num_wallets * 2) as u64);
    let mut funding_txs = Vec::with_capacity(num_wallets);
    for &wallet in &wallets {
        let tx = TransactionRequest::default()
            .with_to(wallet)
            .with_value(funding)
            .with_gas_price(gas_price)
            .with_gas_limit(LOAD_GAS_LIMIT);
        funding_txs.push(tester.l2_provider.send_transaction(tx).await?);
    }
    funding_txs.expect_successful_receipts().await?;
    tracing::info!(num_wallets, %funding, "funded sender wallets");

    // 3. Prime each wallet with one confirmed tx: warms its nonce cache and creates the recipient
    //    account, so the timed window measures steady-state throughput rather than first-tx setup.
    let mut prime_txs = Vec::with_capacity(num_wallets);
    for provider in &providers {
        let tx = build_transfer(recipient, gas_price);
        prime_txs.push(provider.send_transaction(tx).await?);
    }
    prime_txs.expect_successful_receipts().await?;

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
    for provider in providers {
        let sem = sem.clone();
        let submitted = submitted.clone();
        let confirmed = confirmed.clone();
        let latency_micros = latency_micros.clone();
        submitters.push(tokio::spawn(async move {
            let mut receipts = JoinSet::new();
            while Instant::now() < deadline {
                // Acquiring a permit blocks once `concurrency` txs are in flight — backpressure.
                let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
                let tx = build_transfer(recipient, gas_price);
                let sent_at = Instant::now();
                let pending = provider.send_transaction(tx).await?;
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

/// Build a minimal value transfer with a fixed gas price and gas limit so the provider performs no
/// per-transaction gas estimation (only `eth_sendRawTransaction` hits the node).
fn build_transfer(recipient: Address, gas_price: u128) -> TransactionRequest {
    TransactionRequest::default()
        .with_to(recipient)
        .with_value(U256::from(1))
        .with_gas_price(gas_price)
        .with_gas_limit(LOAD_GAS_LIMIT)
}
