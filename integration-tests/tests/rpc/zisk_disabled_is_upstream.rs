//! Disabled-equals-upstream check for the second proof system (ZiSK).
//!
//! With the default config (`second_proof_system = false`) the ZiSK lane must
//! never be entered: the per-block ZiSK input build counter stays 0 across a
//! full pipeline run, and the prover API exposes no ZiSK routes (they 404).
//!
//! This does not need ZiSK enabled or any prover, so it does not depend on the
//! guest ELF. Under the `no-pig` profile (prover input generation disabled) it
//! skips itself.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::NEXT_TO_L1;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;

/// Poll a ZiSK prover-API path and return its HTTP status. When the feature is
/// off the route is not mounted, so this must be 404.
async fn zisk_route_status(client: &reqwest::Client, prover_api_url: &str) -> reqwest::StatusCode {
    client
        .post(format!("{prover_api_url}/prover-jobs/v1/ZiSK/pick"))
        .query(&[("id", "disabled-check")])
        .send()
        .await
        .expect("zisk pick request")
        .status()
}

/// Poll the FRI peek endpoint until a sealed batch appears in the job map,
/// proving the prover-input generator actually ran on the produced blocks.
async fn wait_for_a_fri_job(client: &reqwest::Client, prover_api_url: &str) {
    for _ in 0..240 {
        for batch_number in 1..=4u64 {
            let status = client
                .get(format!(
                    "{prover_api_url}/prover-jobs/v1/FRI/{batch_number}/peek"
                ))
                .send()
                .await
                .expect("fri peek request")
                .status();
            if status == reqwest::StatusCode::OK {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zisk_disabled_is_upstream() -> anyhow::Result<()> {
    if std::env::var("NEXTEST_PROFILE").as_deref() == Ok("no-pig") {
        tracing::warn!("no-pig profile — skipping ZiSK disabled-equals-upstream test");
        return Ok(());
    }

    // Snapshot the process-global counter before any traffic. Under nextest
    // each test runs in its own process, so this is 0; the delta assertion is
    // robust regardless.
    let attempts_before = zksync_os_server::zisk_input_generation_attempts();

    let env = NEXT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    // Force the whole second proof system off so the node runs the upstream
    // path. Both in-process fake provers off so the prover API stays bound and
    // FRI jobs remain peekable.
    config.prover_input_generator_config.second_proof_system = false;
    config.prover_input_generator_config.multi_proof_verifier = false;
    config.prover_input_generator_config.zisk_shadow_execution = false;
    config
        .prover_input_generator_config
        .halt_on_zisk_commitment_mismatch = false;
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
    let tester = env.launch(config).await?;

    let prover_api_url = tester
        .prover_api_url()
        .expect("prover API must be bound when the fake provers are off");

    // Drive real traffic so blocks are produced and the generator runs.
    let recipient: Address = "0xdead000000000000000000000000000000000002".parse()?;
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(recipient)
                .with_value(U256::from(1_000_000_000_000_000_000u128)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let client = reqwest::Client::new();
    // Wait until at least one batch sealed — the generator has now processed
    // real blocks with the feature off.
    wait_for_a_fri_job(&client, &prover_api_url).await;

    // 1. The per-block ZiSK build counter did not move: the lane was never
    //    entered.
    let attempts_after = zksync_os_server::zisk_input_generation_attempts();
    assert_eq!(
        attempts_after, attempts_before,
        "ZiSK input generation must never run when second_proof_system is off \
         (before={attempts_before}, after={attempts_after})"
    );

    // 2. The prover API exposes no ZiSK routes.
    assert_eq!(
        zisk_route_status(&client, &prover_api_url).await,
        reqwest::StatusCode::NOT_FOUND,
        "ZiSK route must 404 when the feature is off"
    );

    Ok(())
}
