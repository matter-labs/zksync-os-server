use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::ext::AnvilApi;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::{CURRENT_TO_L1, TesterBuilder, test_multisetup};
use zksync_os_pipeline_health::{
    BatchPipelineCondition, BlockPipelineCondition, ComponentConditionOverride, ComponentOverrides,
    PipelineHealthConfig,
};

/// Verifies that the /status/health endpoint is reachable and returns a well-formed JSON
/// response containing all expected top-level fields and a valid pipeline snapshot.
#[tokio::test]
async fn health_endpoint_returns_pipeline_snapshot() {
    let node = TesterBuilder::default()
        .build()
        .await
        .expect("failed to start node");

    // Wait for a few blocks to be produced so the pipeline has data
    tokio::time::sleep(Duration::from_millis(200)).await;

    let health = node.get_health().await;

    // Top-level fields must be present
    assert!(
        health.get("status").is_some(),
        "Missing 'status' field in health response; got: {health}"
    );
    assert!(
        health.get("accepting_transactions").is_some(),
        "Missing 'accepting_transactions' field in health response; got: {health}"
    );

    // A freshly started node with no backpressure configured must be accepting transactions.
    let accepting = health["accepting_transactions"]
        .as_bool()
        .expect("'accepting_transactions' must be a bool");
    assert!(
        accepting,
        "Expected node to accept transactions on startup, but it reported not accepting; \
         health response: {health}"
    );

    tracing::info!(
        "Health response:\n{}",
        serde_json::to_string_pretty(&health).unwrap()
    );
}

/// Verifies that the backpressure mechanism fires and clears when L1 block production is stalled.
///
/// # Main-node only
///
/// This test uses `CURRENT_TO_L1` which brings up the full main-node pipeline including the
/// L1Sender components. Backpressure is tied to `l1_sender_commit` block lag, so this test
/// would be meaningless in External Node mode.
///
/// # How the trigger works
///
/// Anvil's automine is disabled after node startup. The `L1SenderCommit` submits its first
/// commit transaction successfully (Anvil accepts it into the mempool) but no block is ever
/// mined, so `get_receipt()` never resolves and `record_processed` is never called.
///
/// Meanwhile the Batcher keeps sealing batches (one per second, with the 1 s `batch_timeout`
/// from the test chain config) and `UpgradeGatekeeper` records each one immediately via
/// `send_and_record` at send time.  The adjacent block lag for `l1_sender_commit`:
///
///   `lag = UpgradeGatekeeper.last_processed − L1SenderCommit.last_processed`
///
/// grows by roughly one batch worth of blocks (~4 blocks at `block_time=250ms`) each second
/// until it crosses the `max_block_lag = 10` threshold.
///
/// Re-enabling Anvil automine causes it to mine all pending transactions.  `L1SenderCommit`
/// receives its receipt, calls `record_processed`, and drains the queued batches.  The lag
/// drops and backpressure clears.
#[test_multisetup([CURRENT_TO_L1])]
async fn backpressure_triggers_and_clears_under_l1_stall(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    // Tight block-lag override on l1_sender_commit only.
    // With batch_timeout=1s and block_time=250ms each batch carries ~4 blocks.
    // UpgradeGatekeeper records at send time; L1SenderCommit records only after mining.
    // The gap crosses 10 within ~3 s of Anvil being disabled.
    let health_config = PipelineHealthConfig {
        component_overrides: ComponentOverrides {
            l1_sender_commit: Some(ComponentConditionOverride {
                enabled: true,
                max_block_lag: Some(10),
                max_time_lag: None,
                max_batch_lag: None,
            }),
            ..ComponentOverrides::default()
        },
        ..PipelineHealthConfig::default()
    };

    let node = builder
        .block_time(Duration::from_millis(250))
        .pipeline_health_config(health_config)
        .build()
        .await?;

    // Disable L1 block production. Anvil accepts transactions into the mempool but
    // does not mine them until automine is re-enabled.
    node.l1_provider()
        .anvil_set_auto_mine(false)
        .await
        .map_err(|e| anyhow::anyhow!("failed to disable Anvil automine: {e}"))?;

    tracing::info!("Anvil automine disabled — waiting for backpressure to fire");

    // --- Phase 1: poll until backpressure fires (max 15 s) ---
    //
    // L1SenderCommit will be stuck waiting for a receipt; UpgradeGatekeeper keeps advancing.
    // The adjacent lag for l1_sender_commit grows ~4 blocks/s and crosses 10 within ~3 s.
    let trigger_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut diag_tick = tokio::time::Instant::now();
    let backpressure_health = loop {
        let health = node.get_health().await;
        let accepting = health["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if !accepting {
            break health;
        }
        if tokio::time::Instant::now() >= trigger_deadline {
            let pipeline = node.get_pipeline().await;
            anyhow::bail!(
                "backpressure did not trigger within 15 s after disabling Anvil; \
                 last health: {health}; pipeline: {pipeline}"
            );
        }
        // Print pipeline state every 3 seconds for diagnosis
        if tokio::time::Instant::now() >= diag_tick {
            let pipeline = node.get_pipeline().await;
            let l1_commit = pipeline["components"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["name"] == "l1_sender_commit"));
            let upgrade_gk = pipeline["components"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["name"] == "upgrade_gatekeeper"));
            tracing::info!(
                l1_sender_commit_adjacent_lag = ?l1_commit.and_then(|c| c["adjacent_block_lag"].as_u64()),
                l1_sender_commit_last_processed = ?l1_commit.and_then(|c| c["last_processed_block"].as_u64()),
                upgrade_gatekeeper_last_processed = ?upgrade_gk.and_then(|c| c["last_processed_block"].as_u64()),
                "DIAG: pipeline state during backpressure wait"
            );
            diag_tick = tokio::time::Instant::now() + Duration::from_secs(3);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let causes = backpressure_health
        .get("causes")
        .and_then(|v| v.as_array())
        .expect("'causes' must be a JSON array when not accepting");
    assert!(
        !causes.is_empty(),
        "Expected at least one backpressure cause; health: {backpressure_health}"
    );
    let commit_cause = causes
        .iter()
        .find(|c| c["component"].as_str() == Some("l1_sender_commit"))
        .unwrap_or_else(|| {
            panic!(
                "Expected a backpressure cause from 'l1_sender_commit'; got: {causes:?}; \
                 full health: {backpressure_health}"
            )
        });
    assert_eq!(
        commit_cause["trigger"].as_str(),
        Some("block_lag_too_high"),
        "Expected l1_sender_commit trigger to be 'block_lag_too_high'; cause: {commit_cause}"
    );

    tracing::info!(
        "Backpressure triggered as expected:\n{}",
        serde_json::to_string_pretty(&backpressure_health).unwrap()
    );

    // --- Phase 1b: FriJobManager in-flight assertion ---
    //
    // When the L1 is stalled and backpressure has fired, FriJobManager may have batches
    // in-flight (proofs being computed). If so, its adjacent_block_lag must be 0: the
    // channel upstream of FriJobManager is empty because it has already picked everything
    // that was sent to it — the stall is downstream (L1SenderCommit), not in the proving
    // channel. A non-zero adjacent lag here would indicate we are misattributing backpressure
    // to FriJobManager instead of the actual bottleneck.
    {
        let pipeline = node.get_pipeline().await;
        if let Some(fri) = pipeline["components"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["name"] == "fri_job_manager"))
            && fri["in_flight_first"].is_object()
        {
            let adjacent_lag = fri["adjacent_block_lag"].as_u64().unwrap_or(0);
            assert_eq!(
                adjacent_lag, 0,
                "FriJobManager adjacent_block_lag should be 0 when batches are in-flight \
                 (channel ahead is empty — bottleneck is downstream at L1SenderCommit); \
                 pipeline: {pipeline}"
            );
        }
    }

    // --- Phase 1c: verify HTTP 503 and RPC rejection while backpressure is active ---
    let raw_response = reqwest::Client::new()
        .get(format!("{}/status/health", node.status_url()))
        .send()
        .await
        .expect("failed to reach /status/health");
    assert_eq!(
        raw_response.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "expected HTTP 503 while backpressure is active"
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

    // --- Phase 2: re-enable Anvil and wait for backpressure to clear (max 30 s) ---
    //
    // Re-enabling automine causes Anvil to mine all pending transactions immediately.
    // L1SenderCommit gets its receipt, records progress, and drains the queued batches.
    // The lag drops below the threshold and backpressure clears.
    node.l1_provider()
        .anvil_set_auto_mine(true)
        .await
        .map_err(|e| anyhow::anyhow!("failed to re-enable Anvil automine: {e}"))?;

    tracing::info!("Anvil automine re-enabled — waiting for backpressure to clear");

    let clear_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let cleared_health = loop {
        let health = node.get_health().await;
        let accepting = health["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if accepting {
            break health;
        }
        if tokio::time::Instant::now() >= clear_deadline {
            anyhow::bail!(
                "backpressure did not clear within 30 s after re-enabling Anvil; \
                 last health: {health}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    assert!(
        cleared_health["accepting_transactions"]
            .as_bool()
            .unwrap_or(false),
        "Expected accepting_transactions=true after backpressure cleared; \
         health: {cleared_health}"
    );

    tracing::info!(
        "Backpressure cleared as expected:\n{}",
        serde_json::to_string_pretty(&cleared_health).unwrap()
    );

    Ok(())
}

/// Verifies that /status/pipeline reflects the configured thresholds per component group.
///
/// Block-pipeline components must expose both max_block_lag and max_time_lag_secs.
/// Batch-pipeline components expose only the thresholds that are configured; in this test
/// batch_pipeline.max_block_lag is None, so batch components must not expose a block-lag
/// threshold. (When batch_pipeline.max_block_lag is set it IS surfaced — see
/// `batch_block_lag_threshold_surfaced_by_pipeline_endpoint`.)
#[tokio::test]
async fn pipeline_endpoint_reflects_configured_thresholds() {
    let health_config = PipelineHealthConfig {
        block_pipeline: BlockPipelineCondition {
            max_block_lag: Some(50),
            max_time_lag: Some(Duration::from_secs(30)),
        },
        batch_pipeline: BatchPipelineCondition {
            max_block_lag: None,
            max_time_lag: Some(Duration::from_secs(300)),
            max_batch_lag: None,
        },
        ..PipelineHealthConfig::default()
    };

    let node = TesterBuilder::default()
        .pipeline_health_config(health_config)
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
        block_executor["thresholds"]["max_block_lag"].as_u64(),
        Some(50),
        "block_executor must expose max_block_lag threshold"
    );
    assert_eq!(
        block_executor["thresholds"]["max_time_lag_secs"].as_f64(),
        Some(30.0),
        "block_executor must expose max_time_lag_secs threshold"
    );

    // batcher is a batch-pipeline component — batch_pipeline.max_block_lag is None in this
    // test, so no block-lag threshold should be surfaced for it.
    let batcher = components
        .iter()
        .find(|c| c["name"].as_str() == Some("batcher"))
        .expect("batcher not found in pipeline components");
    assert!(
        batcher["thresholds"]["max_block_lag"].is_null(),
        "batcher must not expose max_block_lag when batch_pipeline.max_block_lag is None"
    );
    assert_eq!(
        batcher["thresholds"]["max_time_lag_secs"].as_f64(),
        Some(300.0),
        "batcher must expose max_time_lag_secs threshold"
    );
}

/// Verifies that a component_override with enabled=false silences backpressure for that
/// component, even when the group condition would trigger.
///
/// The test uses the same tight batch_pipeline.max_time_lag=500ms that normally fires within
/// ~1 s, but disables it specifically for the batcher. We wait long enough that the batcher
/// lag would normally exceed the threshold, then assert the node remains accepting.
///
/// Note: other batch-pipeline components (e.g. batch_verification) are not disabled here.
/// The test relies on those components not reporting a non-zero timestamp within the short
/// wait window. The goal is to confirm that the batcher specifically does not appear in
/// backpressure_causes when its override is set to enabled=false.
#[test_multisetup([CURRENT_TO_L1])]
async fn component_override_disables_backpressure_for_batcher(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    // Tight threshold on the batch pipeline, but batcher is explicitly silenced.
    let health_config = PipelineHealthConfig {
        batch_pipeline: BatchPipelineCondition {
            max_block_lag: None,
            max_time_lag: Some(Duration::from_millis(500)),
            max_batch_lag: None,
        },
        component_overrides: ComponentOverrides {
            batcher: Some(ComponentConditionOverride {
                enabled: false,
                max_block_lag: None,
                max_time_lag: None,
                max_batch_lag: None,
            }),
            ..ComponentOverrides::default()
        },
        ..PipelineHealthConfig::default()
    };

    let node = builder
        .block_time(Duration::from_millis(250))
        .pipeline_health_config(health_config)
        .build()
        .await?;

    // Wait until the batcher has processed at least one block so the test is not vacuous.
    // If the batcher never processes anything, comp_ts = None, time_lag = None, and
    // backpressure cannot fire regardless of the override — making the absence assertion
    // meaningless. Polling here ensures the batcher actually ran before we assert it did
    // not appear in causes.
    let batcher_ran_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let pipeline = node.get_pipeline().await;
        let batcher_block = pipeline["components"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["name"].as_str() == Some("batcher")))
            .and_then(|c| c["last_processed_block"].as_u64())
            .unwrap_or(0);
        if batcher_block > 0 {
            break;
        }
        if tokio::time::Instant::now() >= batcher_ran_deadline {
            anyhow::bail!(
                "batcher did not process any blocks within 15 s; pipeline is not running"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Wait a bit more so the batcher time-lag would normally exceed the 500ms threshold.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let health = node.get_health().await;

    // The batcher must not appear in any backpressure causes.
    let causes = health
        .get("causes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        causes
            .iter()
            .all(|c| c["component"].as_str() != Some("batcher")),
        "batcher must not appear in causes when its override is enabled=false; \
         health: {health}"
    );

    Ok(())
}

/// Verifies that batch_pipeline.max_block_lag is surfaced correctly by /status/pipeline.
///
/// Batch-pipeline components must expose max_block_lag in their thresholds.
/// Block-pipeline components must NOT pick up batch_pipeline settings.
/// GaplessCommitter is used as the canonical monotonic batch component to
/// confirm the field is threaded all the way through the live node.
#[tokio::test]
async fn batch_block_lag_threshold_surfaced_by_pipeline_endpoint() {
    let health_config = PipelineHealthConfig {
        batch_pipeline: BatchPipelineCondition {
            max_block_lag: Some(500),
            max_time_lag: None,
            max_batch_lag: None,
        },
        ..PipelineHealthConfig::default()
    };

    let node = TesterBuilder::default()
        .pipeline_health_config(health_config)
        .build()
        .await
        .expect("failed to start node");

    let pipeline = node.get_pipeline().await;
    let components = pipeline["components"]
        .as_array()
        .expect("'components' must be a JSON array");

    // GaplessCommitter is a monotonic batch component — must expose max_block_lag.
    let gapless = components
        .iter()
        .find(|c| c["name"].as_str() == Some("gapless_committer"))
        .expect("gapless_committer not found in pipeline components");
    assert_eq!(
        gapless["thresholds"]["max_block_lag"].as_u64(),
        Some(500),
        "gapless_committer must expose batch_pipeline.max_block_lag threshold"
    );
    assert!(
        gapless["thresholds"]["max_time_lag_secs"].is_null(),
        "gapless_committer must not expose max_time_lag_secs when not configured"
    );

    // block_executor is a block-pipeline component — must NOT inherit batch_pipeline settings.
    let block_executor = components
        .iter()
        .find(|c| c["name"].as_str() == Some("block_executor"))
        .expect("block_executor not found in pipeline components");
    assert!(
        block_executor["thresholds"]["max_block_lag"].is_null(),
        "block_executor must not receive max_block_lag from batch_pipeline config"
    );
}

/// Verifies that when block production is disabled via `sequencer_max_blocks_to_produce`,
/// the /status/health endpoint reports `causes` containing a `block_production_disabled` entry
/// and does NOT return an empty `causes` array alongside a non-accepting status.
///
/// # How it works
///
/// Setting `max_blocks_to_produce = 5` causes the BlockExecutor to stop after producing
/// five blocks and send `NotAccepting(BlockProductionDisabled)` on the acceptance channel.
/// The TxAcceptanceGate aggregates this into the combined acceptance state. The health
/// endpoint must reflect it in the `causes` array.
///
/// # Why 5 and not 1
///
/// Node startup (`build()`) calls `wait_for_block(2)` to ensure the test wallet is funded
/// before returning. With a limit of 1, block 2 is never produced and `build()` hangs.
/// Using 5 allows blocks 1–5 to be produced normally (so setup completes), then the
/// sixth Produce command triggers the limit and the health endpoint shows the cause.
#[tokio::test]
async fn block_production_disabled_reported_in_health() {
    let node = TesterBuilder::default()
        .max_blocks_to_produce(5)
        .build()
        .await
        .expect("failed to start node");

    // Keep the mempool non-empty so Produce commands keep making progress.
    // Without this, the first idle Produce after startup can park waiting for
    // transactions and never advance far enough to hit max_blocks_to_produce.
    let tx_load = {
        let provider = node.l2_provider.clone();
        tokio::spawn(async move {
            let gas_price = provider
                .get_gas_price()
                .await
                .expect("failed to fetch gas price for block-production test load");
            loop {
                let tx = TransactionRequest::default()
                    .to(Address::random())
                    .value(U256::from(1u64))
                    .with_gas_price(gas_price * 10);
                let _ = provider.send_transaction(tx).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };

    // Poll until block production stops (accepting_transactions becomes false).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let health = loop {
        let health = node.get_health().await;
        let accepting = health["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if !accepting {
            break health;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("block production did not stop within 30 s; last health: {health}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tx_load.abort();

    // The causes array must be non-empty and contain block_production_disabled.
    let causes = health
        .get("causes")
        .and_then(|v| v.as_array())
        .expect("'causes' must be a JSON array");

    assert!(
        !causes.is_empty(),
        "Expected non-empty 'causes' when block production is disabled; health: {health}"
    );

    assert!(
        causes
            .iter()
            .any(|c| c["trigger"].as_str() == Some("block_production_disabled")),
        "Expected a cause with trigger 'block_production_disabled'; causes: {causes:?}; health: {health}"
    );
}
