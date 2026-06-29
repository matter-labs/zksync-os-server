use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::network::{EthereumWallet, ReceiptResponse, TransactionBuilder, TxSigner};
use alloy::primitives::{Address, Signature, TxKind, U128, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
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
use zksync_os_server::config::FeeConfig;
use zksync_os_interface::traits::EncodedTx;
use zksync_os_sequencer::execution::vm_wrapper::VmWrapper;
use zksync_os_storage_api::{BlockContext, BlockHashes, ReadStateHistory};
use zksync_os_tx_validators::deployment_filter;
use zksync_os_types::{L2Envelope, ZkTransaction, ZksyncOsEncode};

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
    let signer = tester.l2_wallet.default_signer().address();
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let recipient = Address::repeat_byte(0x42);

    // Switch block production over to the direct channel, then send one tx through the normal RPC
    // path. That tx flushes the block producer that is parked waiting on the (now empty) mempool;
    // from the next block on, production pulls exclusively from the direct channel.
    tester.activate_direct_injection();
    let kick = TransactionRequest::default()
        .with_to(recipient)
        .with_value(U256::ZERO)
        .with_gas_price(0)
        .with_gas_limit(LOAD_GAS_LIMIT);
    tester
        .l2_provider
        .send_transaction(kick)
        .await?
        .expect_successful_receipt()
        .await?;

    // Start injecting from the sender's current on-chain nonce (after the kick tx).
    let start_nonce = tester.l2_provider.get_transaction_count(signer).await?;
    // The VM does not validate EOA signatures in forward-running mode and the signer is provided
    // out-of-band, so a fixed dummy signature is fine — this avoids per-tx ECDSA signing entirely.
    let signature = Signature::new(Default::default(), Default::default(), false);

    let submitted = Arc::new(AtomicU64::new(0));
    // Producer-side diagnostics: time spent building txs vs blocked on `send()` (channel full).
    // If send-block dominates, the channel is over-populated (pusher ahead) — the stream is NOT
    // starved. If build dominates / send-block ~0, the pusher itself is the bottleneck.
    let build_micros = Arc::new(AtomicU64::new(0));
    let send_block_micros = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let pusher = tokio::spawn({
        let (sender, submitted, stop) = (sender.clone(), submitted.clone(), stop.clone());
        let (build_micros, send_block_micros) = (build_micros.clone(), send_block_micros.clone());
        async move {
            let mut nonce = start_nonce;
            while !stop.load(Ordering::Relaxed) {
                let build_start = Instant::now();
                let tx = build_direct_tx(chain_id, nonce, recipient, signer, signature);
                build_micros.fetch_add(build_start.elapsed().as_micros() as u64, Ordering::Relaxed);
                // Backpressure: blocks once the channel fills, pacing submission to the sequencer.
                let send_start = Instant::now();
                let send_result = sender.send(tx).await;
                send_block_micros
                    .fetch_add(send_start.elapsed().as_micros() as u64, Ordering::Relaxed);
                if send_result.is_err() {
                    break; // sequencer dropped the channel
                }
                nonce += 1;
                submitted.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

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
    let build_before = build_micros.load(Ordering::Relaxed);
    let send_block_before = send_block_micros.load(Ordering::Relaxed);
    let block_before = tester.l2_provider.get_block_number().await?;
    let measure_start = Instant::now();

    tokio::time::sleep(duration).await;

    let measured = measure_start.elapsed();
    let executed = submitted.load(Ordering::Relaxed) - submitted_before;
    let pusher_build_time =
        Duration::from_micros(build_micros.load(Ordering::Relaxed) - build_before);
    let pusher_send_block_time =
        Duration::from_micros(send_block_micros.load(Ordering::Relaxed) - send_block_before);
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
        // Over the measure window: time the single pusher task spent building txs vs blocked on
        // `send()` (channel full). send_block ≫ build ⇒ pusher is ahead, stream not starved.
        ?pusher_build_time,
        ?pusher_send_block_time,
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
    tracing::info!(base, chain_id, "parallel-blocks bench: base block + chain id");

    let signature = Signature::new(Default::default(), Default::default(), false);
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
        // Pre-encode K disjoint groups of M transfers (untimed). Group i: sender 2i+1 -> recipient
        // 2i+2 (distinct 0xBE addresses), nonces 0..M.
        let groups: Vec<Vec<EncodedTx>> = (0..k)
            .map(|i| {
                let signer = bench_addr(2 * i as u64 + 1);
                let recipient = bench_addr(2 * i as u64 + 2);
                (0..m)
                    .map(|n| build_direct_tx(chain_id, n as u64, recipient, signer, signature).encode())
                    .collect()
            })
            .collect();

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

    // Dummy signature (VM does not validate EOA signatures in forward mode; signer is out-of-band).
    let signature = Signature::new(Default::default(), Default::default(), false);

    let submitted = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // Single pusher round-robining K distinct senders -> K distinct recipients (0xBE-prefixed,
    // disjoint slots), independent per-sender nonce, so `produce_parallel` buckets each batch into K
    // conflict-free groups. Backpressured by the channel, so the submit rate == the pipeline's consume
    // rate. NOTE: this single task tops out near ~1.24M tx/s (cost of `build_direct_tx`); spreading it
    // over K tasks is *slower* (oversubscribes cores vs the executor's VM threads + applier populates,
    // and contends on the channel lock).
    let pusher = tokio::spawn({
        let (sender, submitted, stop) = (sender.clone(), submitted.clone(), stop.clone());
        async move {
            let mut nonces = vec![0u64; k];
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let signer = bench_addr(2 * i as u64 + 1);
                let recipient = bench_addr(2 * i as u64 + 2);
                let tx = build_direct_tx(chain_id, nonces[i], recipient, signer, signature);
                if sender.send(tx).await.is_err() {
                    break; // sequencer dropped the channel
                }
                nonces[i] += 1;
                submitted.fetch_add(1, Ordering::Relaxed);
                i = (i + 1) % k;
            }
        }
    });

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
    let _ = pusher.await;

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
