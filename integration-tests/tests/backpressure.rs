use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::ext::AnvilApi;
use alloy::rpc::types::TransactionRequest;
use serde_json::Value;
use std::time::Duration;
use zksync_os_backpressure::{
    BackpressureConfig, ComponentConditionOverride, ComponentOverrides, PipelineCondition,
};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, TesterBuilder, test_multisetup};

/// Verifies that the /status/accepting endpoint is reachable and returns a well-formed
/// JSON response, and that a freshly started node reports accepting=true.
#[tokio::test]
async fn accepting_endpoint_well_formed_on_startup() {
    let node = TesterBuilder::default()
        .build()
        .await
        .expect("failed to start node");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let accepting_resp = loop {
        let resp = node.get_accepting().await;
        if resp.get("status").is_some() && resp.get("accepting_transactions").is_some() {
            break resp;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "/status/accepting did not return a well-formed response within 10 s; \
                 last: {resp}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let accepting = accepting_resp["accepting_transactions"]
        .as_bool()
        .expect("'accepting_transactions' must be a bool");
    assert!(
        accepting,
        "Expected node to accept transactions on startup, but it reported not accepting; \
         response: {accepting_resp}"
    );

    tracing::info!(
        "Accepting response:\n{}",
        serde_json::to_string_pretty(&accepting_resp).unwrap()
    );
}

/// Runs the L1-stall fixture and asserts the specified `l1_sender_commit` trigger fires
/// and clears. Used by the three sibling tests that exercise block / time / batch
/// triggers against the same real pipeline.
///
/// # Main-node only
///
/// Callers pass `CURRENT_TO_L1` via `test_multisetup` so the full main-node pipeline
/// (including L1Sender) is up — backpressure here is tied to `l1_sender_commit`.
///
/// # How the trigger works
///
/// The Anvil L1 is started with `--mixed-mining` and `block_time(1)` (see
/// `AnvilL1::start`), so both auto-mine and interval mining are active. Disabling just
/// auto-mine is not enough — interval mining would keep producing L1 blocks every
/// second and drain L1 commits. Both must be turned off: `anvil_set_auto_mine(false)`
/// and `anvil_set_interval_mining(0)`. With both off, `L1SenderCommit` submits its
/// first commit transaction successfully but no block is ever mined, so `get_receipt()`
/// never resolves and `record_processed` is not called. Meanwhile `UpgradeGatekeeper`
/// records each batch immediately via `send_and_record`. All three adjacent diffs grow
/// together:
///
///   `lag = UpgradeGatekeeper.last_processed − L1SenderCommit.last_processed`
///
/// With `batch_timeout=1s` and `block_time=250ms`:
///  - block diff grows ~4 blocks/s (one batch carries ~4 blocks)
///  - time diff grows ~1s/s (one batch advances block timestamp by ~1s)
///  - batch diff grows ~1 batch/s
///
/// Re-enabling automine mines everything, `L1SenderCommit` records, and the lag drops.
async fn run_l1_stall_scenario(
    builder: TesterBuilder,
    commit_override: ComponentConditionOverride,
    expected_trigger: &'static str,
    check_actual_vs_threshold: impl Fn(&Value),
) -> anyhow::Result<()> {
    let bp_config = BackpressureConfig {
        component_overrides: ComponentOverrides {
            l1_sender_commit: Some(commit_override),
            ..ComponentOverrides::default()
        },
        ..BackpressureConfig::default()
    };

    let node = builder
        .block_time(Duration::from_millis(250))
        .backpressure_config(bp_config)
        .build()
        .await?;

    // Keep the mempool non-empty so block production keeps running.
    // Without transactions, the block executor waits for pending txs and never advances,
    // meaning no batches get sealed and the adjacent lag never builds up.
    let tx_load = node.spawn_tx_load(Duration::from_millis(100));

    // Wait until L1SenderCommit has processed its first batch. Until then it has neither
    // `last_processed_block` nor `batch_number` populated, which makes `time_diff` and
    // `batch_diff` unrepresentable (both require the downstream coordinate to exist).
    // The block-diff path saturates to zero in this cold-start state, but the other two
    // return None and their thresholds can never fire.
    wait_for_l1_commit_first_processed(&node).await?;

    // Disable L1 block production. Anvil is started with `--mixed-mining` +
    // `block_time(1)`, so both auto-mine and interval mining must be turned off —
    // otherwise interval mining keeps producing L1 blocks every second and commits
    // drain before the lag can build up.
    node.l1_provider()
        .anvil_set_auto_mine(false)
        .await
        .map_err(|e| anyhow::anyhow!("failed to disable Anvil automine: {e}"))?;
    node.l1_provider()
        .anvil_set_interval_mining(0)
        .await
        .map_err(|e| anyhow::anyhow!("failed to disable Anvil interval mining: {e}"))?;

    tracing::info!(
        expected_trigger,
        "Anvil automine disabled — waiting for backpressure to fire"
    );

    let backpressure_resp = wait_for_l1_commit_trigger(&node, expected_trigger).await?;

    let causes = backpressure_resp["causes"]
        .as_array()
        .expect("'causes' must be a JSON array when not accepting");
    let commit_cause = causes
        .iter()
        .find(|c| {
            c["component"].as_str() == Some("l1_sender_commit")
                && c["trigger"].as_str() == Some(expected_trigger)
        })
        .unwrap_or_else(|| {
            panic!(
                "Expected a backpressure cause from 'l1_sender_commit' with trigger \
                 '{expected_trigger}'; got: {causes:?}; full accepting: {backpressure_resp}"
            )
        });
    check_actual_vs_threshold(commit_cause);

    tracing::info!(
        "Backpressure triggered as expected:\n{}",
        serde_json::to_string_pretty(&backpressure_resp).unwrap()
    );

    assert_status_and_rpc_reject_while_backpressured(&node).await?;

    // Re-enable automine first, then interval mining. Anvil 1.5.1 has a bug where
    // calling `anvil_set_interval_mining(N)` while automine is still false does not
    // restart the interval timer — interval mining stays dormant until automine is
    // re-armed. Enabling automine first flushes the mempool (mining any tx submitted
    // during the stall) and arms the miner loop, so the subsequent
    // `anvil_set_interval_mining(1)` properly resumes periodic block production.
    // L1SenderCommit then receives receipts, records progress, drains the queued
    // batches, and backpressure clears.
    node.l1_provider()
        .anvil_set_auto_mine(true)
        .await
        .map_err(|e| anyhow::anyhow!("failed to re-enable Anvil automine: {e}"))?;
    node.l1_provider()
        .anvil_set_interval_mining(1)
        .await
        .map_err(|e| anyhow::anyhow!("failed to re-enable Anvil interval mining: {e}"))?;

    tracing::info!("Anvil automine re-enabled — waiting for backpressure to clear");

    wait_for_backpressure_clear(&node).await?;

    tx_load.abort();
    Ok(())
}

/// Poll `/status/pipeline` until `l1_sender_commit.last_processed_block` is populated —
/// i.e. until at least one commit transaction has been mined on L1 and recorded.
async fn wait_for_l1_commit_first_processed(node: &Tester) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let pipeline = node.get_pipeline().await;
        let l1_commit = pipeline["components"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["name"] == "l1_sender_commit"));
        if let Some(c) = l1_commit
            && c["last_processed_block"].is_u64()
        {
            tracing::info!(
                last_processed_block = c["last_processed_block"].as_u64(),
                last_processed_timestamp = c["last_processed_timestamp"].as_u64(),
                batch_number = c["batch_number"].as_u64(),
                "l1_sender_commit recorded first processed batch"
            );
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "l1_sender_commit did not record any processed batch within 30 s; \
                 pipeline: {pipeline}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll `/status/accepting` until `accepting_transactions=false` and the expected
/// trigger appears on `l1_sender_commit`. Emits `/status/pipeline` diagnostics
/// every 3 s so a timeout produces actionable logs.
async fn wait_for_l1_commit_trigger(
    node: &Tester,
    expected_trigger: &str,
) -> anyhow::Result<Value> {
    let trigger_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut diag_tick = tokio::time::Instant::now();
    loop {
        let resp = node.get_accepting().await;
        let accepting = resp["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if !accepting {
            return Ok(resp);
        }
        if tokio::time::Instant::now() >= trigger_deadline {
            let pipeline = node.get_pipeline().await;
            anyhow::bail!(
                "backpressure ({expected_trigger}) did not trigger within 30 s after \
                 disabling Anvil; last accepting: {resp}; pipeline: {pipeline}"
            );
        }
        if tokio::time::Instant::now() >= diag_tick {
            let pipeline = node.get_pipeline().await;
            let l1_commit = pipeline["components"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["name"] == "l1_sender_commit"));
            let upgrade_gk = pipeline["components"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["name"] == "upgrade_gatekeeper"));
            tracing::info!(
                expected_trigger,
                l1_sender_commit_block_diff_to_upstream = ?l1_commit.and_then(|c| c["block_diff_to_upstream"].as_u64()),
                l1_sender_commit_time_diff_to_upstream_secs = ?l1_commit.and_then(|c| c["time_diff_to_upstream_secs"].as_f64()),
                l1_sender_commit_batch_diff_to_upstream = ?l1_commit.and_then(|c| c["batch_diff_to_upstream"].as_u64()),
                l1_sender_commit_last_processed = ?l1_commit.and_then(|c| c["last_processed_block"].as_u64()),
                upgrade_gatekeeper_last_processed = ?upgrade_gk.and_then(|c| c["last_processed_block"].as_u64()),
                "DIAG: pipeline state during backpressure wait"
            );
            diag_tick = tokio::time::Instant::now() + Duration::from_secs(3);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn assert_status_and_rpc_reject_while_backpressured(node: &Tester) -> anyhow::Result<()> {
    let accepting_raw = reqwest::Client::new()
        .get(format!("{}/status/accepting", node.status_url()))
        .send()
        .await
        .expect("failed to reach /status/accepting");
    assert_eq!(
        accepting_raw.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "expected /status/accepting to return 503 while backpressure is active"
    );

    // /status/ready must stay 200 while backpressure is active: it's a K8s-style
    // readiness probe and must not drain RPC readers from service endpoints on a
    // transient acceptance flip.
    assert_eq!(
        node.get_ready_status().await,
        reqwest::StatusCode::OK,
        "/status/ready must stay 200 while backpressure is active \
         (K8s readiness must not react to acceptance state)"
    );

    let fees = node.l2_provider.estimate_eip1559_fees().await?;
    let tx = TransactionRequest::default()
        .to(Address::random())
        .value(U256::from(1u64))
        .nonce(0)
        .gas_limit(50_000)
        .gas_price(fees.max_fee_per_gas);
    let envelope = tx.build(&node.l2_wallet).await?;
    let encoded = alloy::eips::eip2718::Encodable2718::encoded_2718(&envelope);
    let err = node
        .l2_provider
        .send_raw_transaction(&encoded)
        .await
        .expect_err("transaction should be rejected while backpressure is active");
    assert!(
        err.to_string().contains("pipeline backpressure"),
        "unexpected rejection error: {err}"
    );
    Ok(())
}

async fn wait_for_backpressure_clear(node: &Tester) -> anyhow::Result<()> {
    let clear_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let resp = node.get_accepting().await;
        let accepting = resp["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if accepting {
            tracing::info!(
                "Backpressure cleared as expected:\n{}",
                serde_json::to_string_pretty(&resp).unwrap()
            );
            return Ok(());
        }
        if tokio::time::Instant::now() >= clear_deadline {
            anyhow::bail!(
                "backpressure did not clear within 60 s after re-enabling Anvil; \
                 last accepting: {resp}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Block-diff trigger: the gap crosses `max_block_diff_to_upstream = 5` within ~2 s
/// on fast machines (each batch carries ~4 blocks at `block_time=250ms`). The bound
/// is kept low so that on slow CI — where batches may contain as little as 1 block
/// because block production can't keep up with the 1 s batch timeout — the threshold
/// is still reached inside the 30 s trigger deadline.
#[test_multisetup([CURRENT_TO_L1])]
async fn backpressure_triggers_and_clears_under_l1_stall(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    run_l1_stall_scenario(
        builder,
        ComponentConditionOverride {
            enabled: true,
            max_block_diff_to_upstream: Some(5),
            max_time_diff_to_upstream: None,
            max_batch_diff_to_upstream: None,
        },
        "block_diff_to_upstream_too_high",
        |cause| {
            assert_eq!(
                cause["threshold_blocks"].as_u64(),
                Some(5),
                "cause must carry the configured threshold_blocks; cause: {cause}"
            );
            let actual = cause["actual_blocks"]
                .as_u64()
                .unwrap_or_else(|| panic!("actual_blocks must be present; cause: {cause}"));
            assert!(
                actual > 5,
                "actual_blocks must strictly exceed threshold; got {actual}; cause: {cause}"
            );
        },
    )
    .await
}

/// Time-diff trigger: `UpgradeGatekeeper` records each batch's last block timestamp;
/// `L1SenderCommit` does not advance while Anvil is stalled. With `batch_timeout=1s`
/// each sealed batch advances the upstream timestamp by ~1 s, so
/// `max_time_diff_to_upstream = 3 s` crosses within ~4 s.
#[test_multisetup([CURRENT_TO_L1])]
async fn backpressure_triggers_and_clears_under_l1_stall_time_diff(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    let threshold = Duration::from_secs(3);
    run_l1_stall_scenario(
        builder,
        ComponentConditionOverride {
            enabled: true,
            max_block_diff_to_upstream: None,
            max_time_diff_to_upstream: Some(threshold),
            max_batch_diff_to_upstream: None,
        },
        "time_diff_to_upstream_too_high",
        move |cause| {
            assert_eq!(
                cause["threshold_secs"].as_f64(),
                Some(threshold.as_secs_f64()),
                "cause must carry the configured threshold_secs; cause: {cause}"
            );
            let actual = cause["actual_secs"]
                .as_f64()
                .unwrap_or_else(|| panic!("actual_secs must be present; cause: {cause}"));
            assert!(
                actual > threshold.as_secs_f64(),
                "actual_secs must strictly exceed threshold; got {actual}; cause: {cause}"
            );
        },
    )
    .await
}

/// Batch-diff trigger: `UpgradeGatekeeper` records a batch number each time it hands one off;
/// `L1SenderCommit` uses the `batch_number` fallback because it never calls `record_batch_picked`.
/// With `batch_timeout=1s`, one batch accumulates per second, so `max_batch_diff_to_upstream = 2`
/// crosses within ~3 s. This is the primary end-to-end coverage for the batch-level backpressure
/// path added on this branch (including the L1-Sender fallback).
#[test_multisetup([CURRENT_TO_L1])]
async fn backpressure_triggers_and_clears_under_l1_stall_batch_diff(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    run_l1_stall_scenario(
        builder,
        ComponentConditionOverride {
            enabled: true,
            max_block_diff_to_upstream: None,
            max_time_diff_to_upstream: None,
            max_batch_diff_to_upstream: Some(2),
        },
        "batch_diff_to_upstream_too_high",
        |cause| {
            assert_eq!(
                cause["threshold_batches"].as_u64(),
                Some(2),
                "cause must carry the configured threshold_batches; cause: {cause}"
            );
            let actual = cause["actual_batches"]
                .as_u64()
                .unwrap_or_else(|| panic!("actual_batches must be present; cause: {cause}"));
            assert!(
                actual > 2,
                "actual_batches must strictly exceed threshold; got {actual}; cause: {cause}"
            );
        },
    )
    .await
}

/// Verifies that /status/pipeline reflects the configured thresholds per component group.
///
/// Block-pipeline components must expose both max_block_diff_to_upstream and max_time_diff_to_upstream_secs.
/// Batch-pipeline components expose only the thresholds that are configured; in this test
/// batch_pipeline.max_block_diff_to_upstream is None, so batch components must not expose a
/// block-diff threshold. (When batch_pipeline.max_block_diff_to_upstream is set it IS surfaced — see
/// `batch_block_diff_to_upstream_threshold_surfaced_by_pipeline_endpoint`.)
#[tokio::test]
async fn pipeline_endpoint_reflects_configured_thresholds() {
    let bp_config = BackpressureConfig {
        block_pipeline: PipelineCondition {
            max_block_diff_to_upstream: Some(50),
            max_time_diff_to_upstream: Some(Duration::from_secs(30)),
            max_batch_diff_to_upstream: None,
        },
        batch_pipeline: PipelineCondition {
            max_block_diff_to_upstream: None,
            max_time_diff_to_upstream: Some(Duration::from_secs(300)),
            max_batch_diff_to_upstream: None,
        },
        ..BackpressureConfig::default()
    };

    let node = TesterBuilder::default()
        .backpressure_config(bp_config)
        .build()
        .await
        .expect("failed to start node");

    let pipeline = node.get_pipeline().await;
    let components = pipeline["components"]
        .as_array()
        .expect("'components' must be a JSON array");

    // block_executor is a block-pipeline component — must have both thresholds
    let block_executor = components
        .iter()
        .find(|c| c["name"].as_str() == Some("block_executor"))
        .expect("block_executor not found in pipeline components");
    assert_eq!(
        block_executor["thresholds"]["max_block_diff_to_upstream"].as_u64(),
        Some(50),
        "block_executor must expose max_block_diff_to_upstream threshold"
    );
    assert_eq!(
        block_executor["thresholds"]["max_time_diff_to_upstream_secs"].as_f64(),
        Some(30.0),
        "block_executor must expose max_time_diff_to_upstream_secs threshold"
    );

    // Batcher is classified as block-pipeline: its upstream (ProverInputGenerator) is
    // block-level, so its block and time lag fall in the block-pipeline magnitude range.
    let batcher = components
        .iter()
        .find(|c| c["name"].as_str() == Some("batcher"))
        .expect("batcher not found in pipeline components");
    assert_eq!(
        batcher["thresholds"]["max_block_diff_to_upstream"].as_u64(),
        Some(50),
        "batcher must inherit block_pipeline.max_block_diff_to_upstream"
    );
    assert_eq!(
        batcher["thresholds"]["max_time_diff_to_upstream_secs"].as_f64(),
        Some(30.0),
        "batcher must inherit block_pipeline.max_time_diff_to_upstream"
    );
}

/// Verifies that batch_pipeline.max_block_diff_to_upstream is surfaced correctly by /status/pipeline.
///
/// Batch-pipeline components must expose max_block_diff_to_upstream in their thresholds.
/// Block-pipeline components must NOT pick up batch_pipeline settings.
/// GaplessCommitter is used as the canonical monotonic batch component to
/// confirm the field is threaded all the way through the live node.
#[tokio::test]
async fn batch_block_diff_to_upstream_threshold_surfaced_by_pipeline_endpoint() {
    let bp_config = BackpressureConfig {
        batch_pipeline: PipelineCondition {
            max_block_diff_to_upstream: Some(500),
            max_time_diff_to_upstream: None,
            max_batch_diff_to_upstream: None,
        },
        ..BackpressureConfig::default()
    };

    let node = TesterBuilder::default()
        .backpressure_config(bp_config)
        .build()
        .await
        .expect("failed to start node");

    let pipeline = node.get_pipeline().await;
    let components = pipeline["components"]
        .as_array()
        .expect("'components' must be a JSON array");

    // GaplessCommitter is a monotonic batch component — must expose max_block_diff_to_upstream.
    let gapless = components
        .iter()
        .find(|c| c["name"].as_str() == Some("gapless_committer"))
        .expect("gapless_committer not found in pipeline components");
    assert_eq!(
        gapless["thresholds"]["max_block_diff_to_upstream"].as_u64(),
        Some(500),
        "gapless_committer must expose batch_pipeline.max_block_diff_to_upstream threshold"
    );
    assert!(
        gapless["thresholds"]["max_time_diff_to_upstream_secs"].is_null(),
        "gapless_committer must not expose max_time_diff_to_upstream_secs when not configured"
    );

    // block_executor is a block-pipeline component — must NOT inherit batch_pipeline settings.
    let block_executor = components
        .iter()
        .find(|c| c["name"].as_str() == Some("block_executor"))
        .expect("block_executor not found in pipeline components");
    assert!(
        block_executor["thresholds"]["max_block_diff_to_upstream"].is_null(),
        "block_executor must not receive max_block_diff_to_upstream from batch_pipeline config"
    );
}

/// Verifies that when block production is disabled via `sequencer_max_blocks_to_produce`,
/// the /status/accepting endpoint reports `causes` containing a `block_production_disabled`
/// entry and does NOT return an empty `causes` array alongside a non-accepting status.
///
/// # How it works
///
/// Setting `max_blocks_to_produce = 5` causes the BlockExecutor to stop after producing
/// five blocks and send `NotAccepting(BlockProductionDisabled)` on the acceptance channel.
/// The TxAcceptanceGate aggregates this into the combined acceptance state. The accepting
/// endpoint must reflect it in the `causes` array.
///
/// # Why 5 and not 1
///
/// Node startup (`build()`) calls `wait_for_block(2)` to ensure the test wallet is funded
/// before returning. With a limit of 1, block 2 is never produced and `build()` hangs.
/// Using 5 allows blocks 1–5 to be produced normally (so setup completes), then the
/// sixth Produce command triggers the limit and the accepting endpoint shows the cause.
#[tokio::test]
async fn block_production_disabled_reported_in_accepting() {
    let node = TesterBuilder::default()
        .max_blocks_to_produce(5)
        .build()
        .await
        .expect("failed to start node");

    // Keep the mempool non-empty so Produce commands keep making progress.
    // Without this, the first idle Produce after startup can park waiting for
    // transactions and never advance far enough to hit max_blocks_to_produce.
    let tx_load = node.spawn_tx_load(Duration::from_millis(50));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let accepting_resp = loop {
        let resp = node.get_accepting().await;
        let accepting = resp["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if !accepting {
            break resp;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("block production did not stop within 30 s; last accepting: {resp}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tx_load.abort();

    let causes = accepting_resp
        .get("causes")
        .and_then(|v| v.as_array())
        .expect("'causes' must be a JSON array");

    assert!(
        !causes.is_empty(),
        "Expected non-empty 'causes' when block production is disabled; accepting: {accepting_resp}"
    );

    assert!(
        causes
            .iter()
            .any(|c| c["trigger"].as_str() == Some("block_production_disabled")),
        "Expected a cause with trigger 'block_production_disabled'; causes: {causes:?}; accepting: {accepting_resp}"
    );
}
