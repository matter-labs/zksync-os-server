use std::time::Duration;
use zksync_os_integration_tests::TesterBuilder;

/// Verifies that the /status/health endpoint is reachable and returns a well-formed JSON
/// response containing all expected top-level fields and a valid pipeline snapshot.
///
/// This test validates the end-to-end wiring added in Tasks 8, 9, and 10:
///   - PipelineHealthMonitor is started and wired into the node (Task 8)
///   - PipelineHealthConfig is part of the node Config struct (Task 9)
///   - The /status/health route returns a PipelineSnapshot (Task 10)
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
            entry.get("waiting_send_secs").is_some(),
            "Component entry missing 'waiting_send_secs' field; entry: {entry}"
        );
    }

    tracing::info!(
        "Health response:\n{}",
        serde_json::to_string_pretty(&health).unwrap()
    );
}
