use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
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
    tokio::time::sleep(Duration::from_millis(500)).await;

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

/// Verifies that the backpressure mechanism actually fires and clears under real operation.
///
/// # Main-node only
///
/// This test uses `CURRENT_TO_L1` which brings up the full main-node pipeline including the
/// Batcher. The backpressure condition is tied to Batcher time lag, so this test would be
/// meaningless in External Node mode.
///
/// # How the trigger works
///
/// The Batcher calls
/// `record_processed(last_block_number, Some(last_block_timestamp))` once per batch.
/// The monitor tracks:
///
///   `time_lag = BlockExecutor.last_timestamp - Batcher.last_timestamp`
///
/// Block timestamps are stored as whole Unix seconds. The monitor computes lag as
/// `Duration::from_secs(head_secs - component_secs)`, so the minimum observable
/// non-zero lag is exactly 1 second. Any `max_time_lag` below 1 s (including 500 ms)
/// triggers as soon as the head timestamp is 1 full second ahead of the Batcher's
/// last sealed timestamp. The trigger fires during normal batch accumulation (~1 s
/// batch_timeout) and clears immediately when the next batch seals.
///
/// # Thresholds chosen
///
/// * `batch_pipeline.max_time_lag = 500ms`: any sub-1-second value suffices; we choose
///   500 ms to make the intent clear. The trigger fires once head_ts > batcher_ts by
///   ≥ 1 s (the first integer-second boundary), within the first ~1 s batch cycle.
/// * Trigger poll timeout: 30 s.
/// * Clear poll timeout: 180 s — generous headroom for CI resource contention; clearing
///   happens as soon as the batcher seals the next batch (~1 s after trigger).
#[test_multisetup([CURRENT_TO_L1])]
async fn backpressure_triggers_and_clears_under_batcher_lag(
    builder: TesterBuilder,
) -> anyhow::Result<()> {
    // Configure a tight time-lag threshold on the batch pipeline.
    // Block timestamps are whole Unix seconds. The monitor computes
    // `lag = Duration::from_secs(head_ts - batcher_ts)`, so the minimum non-zero lag is
    // exactly 1 s. Any max_time_lag below 1 s (we use 500 ms) triggers as soon as the head
    // timestamp advances 1 full second past the Batcher's last sealed timestamp (~1 batch cycle).
    let health_config = PipelineHealthConfig {
        batch_pipeline: BatchPipelineCondition {
            max_block_lag: None,
            max_time_lag: Some(Duration::from_millis(500)),
        },
        ..PipelineHealthConfig::default()
    };

    let node = builder
        .block_time(Duration::from_millis(250))
        .pipeline_health_config(health_config)
        .build()
        .await?;

    let tx_load = {
        let provider = node.l2_provider.clone();
        tokio::spawn(async move {
            let gas_price = provider
                .get_gas_price()
                .await
                .expect("failed to fetch gas price for backpressure test load");
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

    // --- Phase 1: confirm the health endpoint is reachable and log initial state ---
    //
    // At startup every component has last_processed_block_number = 0, so the initial lag is 0 and
    // accepting_transactions should be true. However, the node may have produced a handful
    // of blocks during the startup/wait sequence, so we do not make a hard assertion here
    // (the backpressure threshold may already have been reached by the time build() returns).
    let initial_health = node.get_health().await;
    tracing::info!(
        "Initial health after node start:\n{}",
        serde_json::to_string_pretty(&initial_health).unwrap()
    );

    // --- Phase 2: poll until backpressure fires (max 30 s) ---
    //
    // The batcher seals a batch ~every 1 s. Once head_ts advances 1 full second past the
    // Batcher's last sealed timestamp, time_lag (integer seconds) ≥ 1 s > 500 ms threshold,
    // so the monitor sets accepting_transactions=false.
    let trigger_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let backpressure_health = loop {
        let health = node.get_health().await;
        let accepting = health["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if !accepting {
            break health;
        }
        if tokio::time::Instant::now() >= trigger_deadline {
            anyhow::bail!("backpressure did not trigger within 30 s; last health: {health}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    tx_load.abort();

    // The response must carry at least one backpressure cause.
    let causes = backpressure_health
        .get("causes")
        .and_then(|v| v.as_array())
        .expect("'causes' must be a JSON array when not accepting");
    assert!(
        !causes.is_empty(),
        "Expected at least one backpressure cause but got none; health: {backpressure_health}"
    );

    // The Batcher calls record_processed(last_block_number, Some(last_block_timestamp))
    // so the monitor can compute a real time lag and fire backpressure.
    // Other batch-pipeline components may also appear in causes — find the batcher entry.
    let batcher_cause = causes
        .iter()
        .find(|c| c["component"].as_str() == Some("batcher"))
        .unwrap_or_else(|| {
            panic!(
                "Expected a backpressure cause from 'batcher' but got: {causes:?}; \
                 full health: {backpressure_health}"
            )
        });
    assert_eq!(
        batcher_cause["trigger"].as_str(),
        Some("time_lag_too_high"),
        "Expected batcher backpressure trigger to be 'time_lag_too_high'; cause: {batcher_cause}"
    );

    tracing::info!(
        "Backpressure triggered as expected:\n{}",
        serde_json::to_string_pretty(&backpressure_health).unwrap()
    );

    // --- Phase 2b: verify HTTP 503 and RPC rejection while backpressure is active ---
    //
    // The health endpoint must return 503 Service Unavailable when not accepting transactions.
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

    // eth_sendRawTransaction must be rejected with a backpressure error.
    // Build and sign a real transaction following the pattern in tests/rpc/api.rs.
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
    // NotAcceptingReason::PipelineBackpressure formats as:
    // "Node is not currently accepting transactions: pipeline backpressure (N component(s) reporting)."
    assert!(
        err.to_string().contains("pipeline backpressure"),
        "unexpected rejection error: {err}"
    );

    // --- Phase 3: wait for backpressure to clear naturally (max 180 s) ---
    //
    // Once the next batch seals (~1 s after trigger), Batcher calls record_processed and the
    // lag drops below the threshold. Clearing does not wait for the ProverInputGenerator to
    // finish — it happens at batch seal time. The 180 s deadline is generous headroom for CI
    // resource contention.
    let clear_deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let cleared_health = loop {
        let health = node.get_health().await;
        let accepting = health["accepting_transactions"]
            .as_bool()
            .expect("'accepting_transactions' must be a bool");
        if accepting {
            break health;
        }
        if tokio::time::Instant::now() >= clear_deadline {
            anyhow::bail!("backpressure did not clear within 180 s; last health: {health}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
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
        },
        component_overrides: ComponentOverrides {
            batcher: Some(ComponentConditionOverride {
                enabled: false,
                max_block_lag: None,
                max_time_lag: None,
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
    let batcher_ran_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
                "batcher did not process any blocks within 30 s; pipeline is not running"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Wait a bit more so the batcher time-lag would normally exceed the 500ms threshold.
    tokio::time::sleep(Duration::from_secs(3)).await;

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
