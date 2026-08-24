#![cfg(feature = "gpu-prover-tests")]
//! Live end-to-end test for the zksync-os 0.5.0 lane (protocol v32.0, execution V7,
//! proving V8, native batch PIG):
//!
//! 1. Start a v31.0 chain settling on L1 with fake FRI/SNARK provers, then perform
//!    a protocol upgrade to v32.0.
//! 2. Wait for the fake pipeline to settle everything produced so far.
//! 3. Restart the node with fake FRI provers disabled and spawn the released
//!    `zksync-os-prover-service` (zksync-airbender-prover) against the node's prover API.
//! 4. Success when a post-restart transaction's block is finalized — i.e. its batch was
//!    committed, FRI-proven for real (proof verified by the server), fake-SNARKed and
//!    proven+executed on L1.
//!
//! Required environment:
//!   COMPACT_CRS_FILE         path to the compact CRS (the prover service demands it up front)
//! Optional:
//!   V8_PROVING_TIMEOUT_SECS  how long to wait for the real proof (default 30m, GPU-shaped)
//!   V8_FRI_PROVER_BIN        prover service binary to use instead of the released one
//!   V8_APP_BIN               V8 `multiblock_batch.bin` to use instead of the released one
//!                            (its `.text` sibling must sit next to it)

use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use zksync_os_integration_tests::TestCase;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::upgrade::UpgradeTester;
use zksync_os_server::default_protocol_version::PROTOCOL_VERSION_V31_0;

#[test_log::test(tokio::test)]
async fn v8_native_pig_real_fri_proof_e2e() -> anyhow::Result<()> {
    let proving_timeout = Duration::from_secs(
        std::env::var("V8_PROVING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30 * 60),
    );

    // Phase 1: v31.0 chain settling on L1; fake FRI + SNARK provers keep the
    // pipeline moving.
    let tester = TestCase {
        protocol_version: PROTOCOL_VERSION_V31_0,
    }
    .environment()
    .await?
    .launch_default()
    .await?;

    // Upgrade v31.0 -> v32.0 (execution V7 / proving V8).
    {
        let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;
        let protocol_upgrade = upgrade_tester
            .protocol_upgrade_builder()
            .await?
            .bump_minor(1)
            .with_force_deployments(BTreeMap::new())
            .with_timestamp(U256::from(1))
            .build();
        upgrade_tester
            .execute_default_upgrade(
                &protocol_upgrade,
                U256::MAX,
                U256::from(1),
                false,
                zksync_os_integration_tests::upgrade::v32_facet_cuts(&upgrade_tester).await?,
                Some(
                    zksync_os_integration_tests::upgrade::ZKSYNC_OS_TESTNET_VERIFIER_DEPLOYED_BYTECODE
                        .parse::<alloy::primitives::Bytes>()?,
                ),
            )
            .await?;
    }
    tracing::info!("protocol upgrade to v32.0 executed");

    // Flush the fake-proven tail: a post-upgrade tx must execute on L1 before we switch to
    // real proving, so every batch produced so far (v31.0 and early v32.0) is settled.
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?
        .expect_to_execute()
        .await?;
    tracing::info!("post-upgrade tx executed on L1; all earlier batches settled");

    // Phase 2: restart the node with real FRI proving. Fake SNARK provers stay on
    // (no GPU/CRS here), so finalization of the probe tx requires exactly one real
    // V8 FRI proof.
    let tester = tester
        .stop()
        .await?
        .start_with_overrides(|config| {
            // Phase-1 launch disables the prover HTTP API when both fake pools are on
            // (see `launch_node_inner`); re-enable it for the external prover.
            config.prover_api_config.enabled = true;
            config.prover_api_config.fake_fri_provers.enabled = false;
            config.prover_api_config.fake_snark_provers.enabled = true;
            // A CPU prover holds its job for hours; never reassign it mid-proving.
            config.prover_api_config.fri_job_timeout = Duration::from_secs(48 * 3600);
            // Generous first-batch window so the probe tx below lands in the first
            // post-restart batch (a stray empty batch would cost CPU-hours to prove).
            config.batcher_config.batch_timeout = Duration::from_secs(120);
        })
        .await?;

    // The tx we expect to finalize through a real V8 FRI proof. Poll for the receipt
    // manually instead of using the pending-tx watcher: right after a node restart the
    // watcher's subscription can miss the inclusion notification and time out even though
    // the tx was mined within seconds.
    let pending = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?;
    let tx_hash = *pending.tx_hash();
    drop(pending);
    let receipt = {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if let Some(receipt) = tester.l2_provider.get_transaction_receipt(tx_hash).await? {
                break receipt;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "probe tx {tx_hash} was not mined within 300s"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    anyhow::ensure!(receipt.status(), "probe tx {tx_hash} reverted");
    let block_number = receipt
        .block_number
        .expect("mined receipt is missing block number");

    let prover_api_url = tester
        .prover_api_url()
        .expect("prover API must be enabled on the test node");
    tracing::info!(
        prover_api_url,
        block_number,
        "starting external V8 FRI prover"
    );

    let output_dir = tempfile::tempdir()?;
    let mut prover =
        zksync_os_integration_tests::spawn_v8_prover_service(&prover_api_url, output_dir.path())
            .await;

    // Wait until the probe tx's block is executed on L1 (`finalized` maps to executed).
    let deadline = Instant::now() + proving_timeout;
    loop {
        // The service is spawned without `--iterations`, so it never exits on its own.
        if let Some(status) = prover.try_wait()? {
            anyhow::bail!("external FRI prover exited prematurely with {status}");
        }
        let finalized = tester
            .l2_provider
            .get_block_number_by_id(BlockId::finalized())
            .await?;
        if finalized >= Some(block_number) {
            tracing::info!(
                ?finalized,
                block_number,
                "probe tx block finalized on L1 via real V8 FRI proof"
            );
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "block {block_number} was not finalized within {proving_timeout:?} \
             (last finalized: {finalized:?})"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let _ = prover.kill().await;
    Ok(())
}
