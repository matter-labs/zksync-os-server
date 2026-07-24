//! L1 sender behavior under load and adverse L1 conditions: pipelined-vs-stop-and-wait
//! throughput, base/priority fee spikes, inclusion stalls, restarts mid-window,
//! forced resubmission and L1 connection loss.

use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::ext::AnvilApi;
use alloy::rpc::types::TransactionRequest;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL, ReceiptAssert};
use zksync_os_integration_tests::l1_helpers::{fetch_l1_state, wait_for_l1_state};
use zksync_os_integration_tests::test_config::make_full_pipeline_config;
use zksync_os_integration_tests::{CURRENT_TO_L1, TestEnvironment, Tester, test_multisetup};
use zksync_os_provider::NodeProvider;
use zksync_os_server::config::Config;

/// Seals a batch roughly every two L2 transactions (and every 2s by timeout), so a burst of
/// transactions turns into a stream of batches — i.e. of L1 commit transactions.
fn fast_batches_config(config: &mut Config) {
    make_full_pipeline_config(config);
    config.prover_api_config.fake_fri_provers.compute_time = Duration::ZERO;
    // Both prover stages are faked, so prover inputs may be faked too (independent of the
    // NEXTEST_PROFILE the suite happens to run under).
    config.prover_input_generator_config.enable_input_generation = false;
    config.sequencer_config.block_time = Duration::from_millis(50);
    config.batcher_config.tx_per_batch_limit = 1;
    config.batcher_config.batch_timeout = Duration::from_secs(2);
}

async fn send_l2_txs(tester: &Tester, count: usize) -> anyhow::Result<()> {
    for _ in 0..count {
        tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(1u64)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
    }
    Ok(())
}

async fn commit_operator_address(config: &Config) -> anyhow::Result<Address> {
    config
        .l1_sender_config
        .operator_commit_sk
        .as_ref()
        .expect("test config carries a commit operator key")
        .address()
        .await
}

/// (latest, pending) nonces of `address`; `pending - latest` = txs in the anvil mempool.
async fn operator_nonces(l1: &NodeProvider, address: Address) -> anyhow::Result<(u64, u64)> {
    let latest = l1.get_transaction_count(address).await?;
    let pending = l1.get_transaction_count(address).pending().await?;
    Ok((latest, pending))
}

/// Fully stops anvil block production (both tx-triggered and interval mining).
async fn stop_l1_mining(l1: &NodeProvider) -> anyhow::Result<()> {
    l1.anvil_set_auto_mine(false).await?;
    l1.anvil_set_interval_mining(0).await?;
    Ok(())
}

/// Resumes anvil block production and immediately mines a burst so pooled transactions land
/// and confirmation depths advance.
async fn resume_l1_mining(l1: &NodeProvider) -> anyhow::Result<()> {
    l1.anvil_set_auto_mine(true).await?;
    l1.anvil_set_interval_mining(1).await?;
    l1.anvil_mine(Some(50), None).await?;
    Ok(())
}

/// Waits until the commit operator has exactly `expected` transactions pooled on L1.
async fn wait_for_pooled_commits(
    l1: &NodeProvider,
    operator: Address,
    expected: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        let (latest, pending) = operator_nonces(l1, operator).await?;
        if pending - latest == expected {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for {expected} pooled commit txs; have {}",
            pending - latest,
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The core throughput claim: with everything else equal, the pipelined sender drains a batch
/// backlog to L1 several times faster than the stop-and-wait sender, because it never spends
/// the inclusion + `required_confirmations` wait doing nothing.
///
/// Construction: the backlog is built while L1 mining is stopped, then the measurement covers
/// only the drain after mining resumes at ~1s blocks. `required_confirmations = 24` makes the
/// per-cycle dead time the stop-and-wait design pays (~24s per cycle at 1s blocks) dominate,
/// mimicking the mainnet ratio of confirmation-wait to batch demand; the pipelined sender only
/// pays that wait once, off the submission path.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn pipelined_commit_sender_outpaces_stop_and_wait(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    const TXS: usize = 60;
    const TARGET_NEW_BATCHES: u64 = 24;
    const CONFIRMATIONS: u64 = 24;

    async fn run_variant(env: TestEnvironment, pipelined: bool) -> anyhow::Result<Duration> {
        let mut config = env.default_config().await?;
        fast_batches_config(&mut config);
        config.l1_sender_config.required_confirmations = CONFIRMATIONS;
        config.l1_sender_config.pipelining_enabled = pipelined;
        let tester = env.launch(config).await?;
        let l1 = tester.l1_provider().clone();

        let initial = fetch_l1_state(&tester).await?.last_committed_batch;

        // Build the backlog with L1 inclusion stalled, so the measurement below captures
        // sender throughput rather than L2 batch production.
        stop_l1_mining(&l1).await?;
        send_l2_txs(&tester, TXS).await?;
        tokio::time::sleep(Duration::from_secs(8)).await;

        let started = Instant::now();
        // Resume without a catch-up burst: confirmations must accrue at the real block
        // cadence for the comparison to be meaningful.
        l1.anvil_set_auto_mine(true).await?;
        l1.anvil_set_interval_mining(1).await?;
        let target = initial + TARGET_NEW_BATCHES;
        wait_for_l1_state(&tester, "backlog drained to L1", |state| {
            state.last_committed_batch >= target
        })
        .await?;
        let elapsed = started.elapsed();
        anyhow::ensure!(!tester.has_crashed(), "node crashed during the benchmark");
        tester.shutdown().await?;
        Ok(elapsed)
    }

    let legacy_env = CURRENT_TO_L1.environment().await?;
    let pipelined_elapsed = run_variant(env, true).await?;
    let legacy_elapsed = run_variant(legacy_env, false).await?;

    let speedup = legacy_elapsed.as_secs_f64() / pipelined_elapsed.as_secs_f64();
    println!(
        "L1 commit throughput ({TARGET_NEW_BATCHES}-batch backlog drained): \
         pipelined {pipelined_elapsed:.2?} vs stop-and-wait {legacy_elapsed:.2?} \
         => {speedup:.2}x speedup"
    );
    assert!(
        speedup >= 2.5,
        "expected the pipelined sender to be at least 2.5x faster \
         (pipelined {pipelined_elapsed:?}, stop-and-wait {legacy_elapsed:?}, {speedup:.2}x)",
    );
    Ok(())
}

/// A base-fee spike above the operator's configured `max_fee_per_gas` cap must stall the
/// sender (retry-and-wait), not crash the node; once the fee market decays back under the
/// cap, commits resume on their own.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn base_fee_spike_stalls_sender_without_crashing(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    let tester = env.launch(config).await?;
    let l1 = tester.l1_provider().clone();

    // Baseline: commits flow.
    let initial = fetch_l1_state(&tester).await?.last_committed_batch;
    send_l2_txs(&tester, 6).await?;
    wait_for_l1_state(&tester, "baseline commits", |state| {
        state.last_committed_batch >= initial + 2
    })
    .await?;

    // Spike the base fee to 5000 gwei — far above the 200 gwei default cap. Anvil rejects
    // under-priced submissions outright, so the sender must enter its fee-wait loop.
    // Empty blocks decay the base fee by 12.5% each, so at 0.25s blocks the market comes
    // back under the cap in roughly 25 blocks (~7s).
    l1.anvil_set_next_block_base_fee_per_gas(5_000_000_000_000u128)
        .await?;
    let spiked = fetch_l1_state(&tester).await?.last_committed_batch;

    // The node keeps accepting L2 traffic and stays alive through the spike.
    send_l2_txs(&tester, 10).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !tester.has_crashed(),
        "node crashed during a base-fee spike; the sender should stall and retry instead",
    );

    // After the decay, everything produced during the spike settles.
    wait_for_l1_state(&tester, "commits resumed after the spike", |state| {
        state.last_committed_batch >= spiked + 6
    })
    .await?;
    assert!(!tester.has_crashed());
    Ok(())
}

/// When L1 stops including transactions entirely (mining halt — the extreme version of blob
/// space exhaustion), the sender must fill its in-flight window up to exactly
/// `command_limit` pooled transactions, never exceed it (the L1 per-account pool cap), stay
/// alive, and drain everything once inclusion resumes.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn inclusion_stall_bounds_in_flight_window_and_recovers(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    const WINDOW: u64 = 8;

    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    config.l1_sender_config.command_limit = WINDOW as usize;
    let tester = env.launch(config).await?;
    let l1 = tester.l1_provider().clone();
    let operator = commit_operator_address(tester.config()).await?;

    let initial = fetch_l1_state(&tester).await?.last_committed_batch;
    send_l2_txs(&tester, 4).await?;
    wait_for_l1_state(&tester, "baseline commits", |state| {
        state.last_committed_batch > initial
    })
    .await?;

    stop_l1_mining(&l1).await?;
    send_l2_txs(&tester, 30).await?;

    // The window must fill to the cap...
    wait_for_pooled_commits(&l1, operator, WINDOW).await?;
    // ...and never exceed it while inclusion is stalled.
    for _ in 0..15 {
        let (latest, pending) = operator_nonces(&l1, operator).await?;
        assert!(
            pending - latest <= WINDOW,
            "in-flight window exceeded the per-account cap: {} > {WINDOW}",
            pending - latest,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !tester.has_crashed(),
        "node crashed during an L1 inclusion stall"
    );

    resume_l1_mining(&l1).await?;
    wait_for_l1_state(&tester, "stalled batches drained", |state| {
        state.last_committed_batch >= initial + 12
    })
    .await?;
    assert!(!tester.has_crashed());
    Ok(())
}

/// Crash-with-full-window is the common failure mode of a pipelined sender. The node is
/// stopped while `command_limit` commit transactions sit unmined in the L1 mempool; mining
/// resumes while the node is starting back up, so the in-flight commits land mid-startup —
/// exactly the window in which a commit event must not trip the `UnexpectedCommit` guard —
/// and every batch must settle exactly once (a duplicate commit would revert on L1 and crash
/// the node).
///
/// (Startup L1-state discovery deliberately waits for pool-driven contract-state changes to
/// land, so mining must resume for the node to boot at all — anvil cannot hold a
/// "pooled txs + progressing chain" state that would exercise in-pool pairing.)
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn restart_mid_window_settles_in_flight_batches_exactly_once(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    const WINDOW: u64 = 8;

    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    config.l1_sender_config.command_limit = WINDOW as usize;
    let tester = env.launch(config).await?;
    let l1 = tester.l1_provider().clone();
    let operator = commit_operator_address(tester.config()).await?;

    let initial = fetch_l1_state(&tester).await?.last_committed_batch;
    send_l2_txs(&tester, 4).await?;
    wait_for_l1_state(&tester, "baseline commits", |state| {
        state.last_committed_batch > initial
    })
    .await?;

    // Fill the in-flight window with pooled (unmined) commit txs, then stop the node while
    // they are still pending.
    stop_l1_mining(&l1).await?;
    send_l2_txs(&tester, 24).await?;
    wait_for_pooled_commits(&l1, operator, WINDOW).await?;
    let stopped = tester.stop().await?;

    // Resume mining while the node is starting: the pooled commits mine mid-startup, after
    // the commit watcher is armed but before the sender's recovery runs.
    let resume_l1 = l1.clone();
    let resume = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        resume_l1_mining(&resume_l1).await
    });
    let restarted = stopped.start().await?;
    resume
        .await
        .expect("resume task panicked")
        .expect("failed to resume L1 mining");

    wait_for_l1_state(&restarted, "in-flight batches settled", |state| {
        state.last_committed_batch >= initial + 10
    })
    .await?;
    // Still alive: no duplicate commit reverted on L1 and the UnexpectedCommit guard did not
    // fire for transactions the previous session left in flight.
    assert!(!restarted.has_crashed());
    let (latest, pending) = operator_nonces(&l1, operator).await?;
    assert!(
        pending - latest <= WINDOW,
        "in-flight window exceeded after restart: {} > {WINDOW}",
        pending - latest,
    );
    Ok(())
}

/// The operator escape hatch for transactions dropped from the L1 mempool: a base-fee spike
/// evicts the pooled (cap-priced) commit txs from anvil's pool; a restart with
/// `force_transaction_resubmission` must resubmit the queued commands from the confirmed
/// nonce with replacement fees and settle every batch exactly once.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn force_resubmission_resends_dropped_transactions(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    const WINDOW: u64 = 8;

    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    config.l1_sender_config.command_limit = WINDOW as usize;
    let tester = env.launch(config).await?;
    let l1 = tester.l1_provider().clone();
    let operator = commit_operator_address(tester.config()).await?;

    let initial = fetch_l1_state(&tester).await?.last_committed_batch;
    send_l2_txs(&tester, 4).await?;
    wait_for_l1_state(&tester, "baseline commits", |state| {
        state.last_committed_batch > initial
    })
    .await?;

    stop_l1_mining(&l1).await?;
    send_l2_txs(&tester, 24).await?;
    wait_for_pooled_commits(&l1, operator, WINDOW).await?;
    let (latest_before, _) = operator_nonces(&l1, operator).await?;
    let stopped = tester.stop().await?;

    // A base-fee spike above the txs' 200 gwei cap makes anvil evict them from the pool once
    // mining resumes ("dropped from the mempool"). 2000 gwei decays below the 400 gwei
    // replacement cap in ~12 blocks.
    l1.anvil_set_next_block_base_fee_per_gas(2_000_000_000_000u128)
        .await?;
    l1.anvil_set_auto_mine(true).await?;
    l1.anvil_set_interval_mining(1).await?;
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        let (latest, pending) = operator_nonces(&l1, operator).await?;
        if pending == latest {
            assert_eq!(
                latest, latest_before,
                "cap-priced txs should be evicted, not mined, under the spiked base fee",
            );
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for the pool to evict under-priced commit txs"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let mut config = stopped.config().clone();
    config
        .l1_sender_config
        .force_transaction_resubmission
        .enabled = true;
    let restarted = stopped.start_with_config(config).await?;

    wait_for_l1_state(
        &restarted,
        "dropped batches resubmitted and settled",
        |state| state.last_committed_batch >= initial + 10,
    )
    .await?;
    assert!(!restarted.has_crashed());
    Ok(())
}

/// A priority-fee spike on L1 (other actors bidding high tips) must never push the sender's
/// bids above its configured priority-fee cap, and commits keep flowing.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn priority_fee_spike_is_capped_and_commits_continue(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    let tester = env.launch(config).await?;
    let l1 = tester.l1_provider().clone();
    let operator = commit_operator_address(tester.config()).await?;
    let priority_fee_cap = tester.config().l1_sender_config.max_priority_fee_per_gas.0;

    let initial = fetch_l1_state(&tester).await?.last_committed_batch;

    // Skew the fee-history percentiles with high-tip L1 transactions while the sender works.
    for _ in 0..20 {
        l1.send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64))
                .with_max_fee_per_gas(300_000_000_000u128)
                .with_max_priority_fee_per_gas(100_000_000_000u128),
        )
        .await?
        .get_receipt()
        .await?;
    }
    send_l2_txs(&tester, 10).await?;
    wait_for_l1_state(&tester, "commits during tip spike", |state| {
        state.last_committed_batch >= initial + 4
    })
    .await?;

    // The last mined commit tx must have honored the configured cap despite the spiked
    // fee-history estimate.
    let (latest, _) = operator_nonces(&l1, operator).await?;
    anyhow::ensure!(latest > 0, "no commit txs mined");
    let last_commit_tx = l1
        .get_transaction_by_sender_nonce(operator, latest - 1)
        .await?
        .expect("mined commit tx must be retrievable");
    let tip = ConsensusTransaction::max_priority_fee_per_gas(&last_commit_tx).unwrap_or_default();
    assert!(
        tip <= priority_fee_cap,
        "commit tx bid a priority fee above the configured cap: {tip} > {priority_fee_cap}",
    );
    assert!(!tester.has_crashed());
    Ok(())
}

/// A TCP proxy in front of anvil that can be "unplugged": while paused it kills active
/// connections and refuses new ones, producing the same transport errors as a flaky L1 RPC.
struct FlakyL1Proxy {
    url: String,
    paused: Arc<AtomicBool>,
    epoch_tx: Arc<tokio::sync::watch::Sender<u64>>,
}

impl FlakyL1Proxy {
    async fn start(target_url: &str) -> anyhow::Result<Self> {
        let target = target_url
            .strip_prefix("http://")
            .unwrap_or(target_url)
            .trim_end_matches('/')
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let paused = Arc::new(AtomicBool::new(false));
        let (epoch_tx, _) = tokio::sync::watch::channel(0u64);
        let epoch_tx = Arc::new(epoch_tx);

        let accept_paused = Arc::clone(&paused);
        let accept_epoch = Arc::clone(&epoch_tx);
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                if accept_paused.load(Ordering::SeqCst) {
                    // Refuse service while "unplugged".
                    continue;
                }
                let target = target.clone();
                let mut epoch_rx = accept_epoch.subscribe();
                tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        // A pause bump severs live connections mid-flight.
                        _ = epoch_rx.changed() => {}
                    }
                });
            }
        });

        Ok(Self {
            url,
            paused,
            epoch_tx,
        })
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.epoch_tx.send_modify(|epoch| *epoch += 1);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
}

/// A short L1 connection outage (dropped and refused connections) must be absorbed by the
/// provider's transport retries: no crash, and settlement resumes once connectivity is back.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn l1_connection_blip_is_absorbed(env: TestEnvironment) -> anyhow::Result<()> {
    let proxy = FlakyL1Proxy::start(env.l1_rpc_url()).await?;
    let mut config = env.default_config().await?;
    fast_batches_config(&mut config);
    let tester = env.launch_with_l1_rpc(config, proxy.url.clone()).await?;

    let initial = fetch_l1_state(&tester).await?.last_committed_batch;
    send_l2_txs(&tester, 6).await?;
    wait_for_l1_state(&tester, "baseline commits", |state| {
        state.last_committed_batch >= initial + 2
    })
    .await?;

    // 2s outage: within the transport retry budget (5 retries, 1s backoff).
    proxy.pause();
    tokio::time::sleep(Duration::from_secs(2)).await;
    proxy.resume();

    send_l2_txs(&tester, 6).await?;
    wait_for_l1_state(&tester, "commits after the connection blip", |state| {
        state.last_committed_batch >= initial + 5
    })
    .await?;
    assert!(
        !tester.has_crashed(),
        "node crashed on a short L1 connection blip that transport retries should absorb",
    );
    Ok(())
}
