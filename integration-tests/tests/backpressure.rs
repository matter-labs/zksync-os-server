use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::{CURRENT_TO_L1, TesterBuilder, test_multisetup};
use zksync_os_pipeline_health::{BatchPipelineCondition, PipelineHealthConfig};

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
        health.get("healthy").is_some(),
        "Missing 'healthy' field in health response; got: {health}"
    );
    assert!(
        health.get("accepting_transactions").is_some(),
        "Missing 'accepting_transactions' field in health response; got: {health}"
    );

    // Pipeline snapshot must be present
    let pipeline = health
        .get("pipeline")
        .expect("Missing 'pipeline' key in health response");

    assert!(
        pipeline.get("head_block").is_some(),
        "Missing 'pipeline.head_block' in health response; got: {health}"
    );
    assert!(
        pipeline
            .get("components")
            .and_then(|v| v.as_array())
            .is_some(),
        "Missing or non-array 'pipeline.components' in health response; got: {health}"
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

    // head_block is reported as a non-negative integer (u64)
    let head_block = pipeline["head_block"]
        .as_u64()
        .expect("'pipeline.head_block' must be a u64-compatible integer");
    // Genesis block produces block 0; after startup the node should have produced at least one
    // block (the upgrade transaction block). We allow 0 here since the timing may vary in CI.
    let _ = head_block; // value is valid; just checking type and presence

    // Each component entry must have required fields
    let components = pipeline["components"].as_array().unwrap();
    for entry in components {
        assert!(
            entry.get("name").and_then(|v| v.as_str()).is_some(),
            "Component entry missing 'name' field; entry: {entry}"
        );
        assert!(
            entry.get("state").and_then(|v| v.as_str()).is_some(),
            "Component entry missing 'state' field; entry: {entry}"
        );
        assert!(
            entry.get("last_processed_block").is_some(),
            "Component entry missing 'last_processed_block' field; entry: {entry}"
        );
        assert!(
            entry.get("block_lag").is_some(),
            "Component entry missing 'block_lag' field; entry: {entry}"
        );
        assert!(
            entry.get("time_lag_secs").is_some(),
            "Component entry missing 'time_lag_secs' field; entry: {entry}"
        );
    }

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
        .get("backpressure_causes")
        .and_then(|v| v.as_array())
        .expect("'backpressure_causes' must be a JSON array when not accepting");
    assert!(
        !causes.is_empty(),
        "Expected at least one backpressure cause but got none; health: {backpressure_health}"
    );

    // The cause must come from the batcher with a time-lag trigger.
    // The Batcher calls record_processed(last_block_number, Some(last_block_timestamp))
    // so the monitor can compute a real time lag and fire backpressure.
    let cause = &causes[0];
    assert_eq!(
        cause["component"].as_str(),
        Some("batcher"),
        "Expected backpressure to originate from 'batcher'; cause: {cause}"
    );
    assert_eq!(
        cause["trigger"].as_str(),
        Some("time_lag_too_high"),
        "Expected backpressure trigger to be 'time_lag_too_high'; cause: {cause}"
    );

    tracing::info!(
        "Backpressure triggered as expected:\n{}",
        serde_json::to_string_pretty(&backpressure_health).unwrap()
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
