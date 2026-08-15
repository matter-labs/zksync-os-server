//! Cross-lane witness consistency on a v31 chain — CPU only.
//!
//! The multiprover property, checked without provers: for every batch, the
//! Airbender witness replayed through the v31 app binary on the RISC-V
//! simulator must reproduce the same batch public input that the ZiSK/REVM
//! guest computes from its own witness. A mismatch here means one of the
//! lanes (or the batcher) would reject or fail to prove the batch on real
//! hardware — this test surfaces that in minutes, before any GPU time.

use std::time::Duration;

use alloy::primitives::{B256, keccak256};
use base64::Engine;
use zksync_os_integration_tests::CURRENT_TO_L1;
use zksync_os_integration_tests::test_config::enable_second_proof_system;
use zksync_os_types::ProvingVersion;

/// Sealed batches to capture and cross-check. Batch 1 is the genesis batch;
/// the rest carry the driven transfers.
const BATCHES: u64 = 3;

/// One batch's captured prover inputs.
struct CapturedBatch {
    vk_hash: String,
    airbender_witness: Vec<u32>,
    zisk_input: Vec<u8>,
}

async fn peek_json(url: String) -> anyhow::Result<Option<serde_json::Value>> {
    let response = reqwest::Client::new().get(url).send().await?;
    if response.status() != reqwest::StatusCode::OK {
        return Ok(None);
    }
    Ok(Some(response.json().await?))
}

/// Poll both peek endpoints for `batch` until present (or `deadline` passes).
/// No prover consumes jobs in this configuration, so a job that appears stays
/// peekable.
async fn capture_batch(
    prover_api_url: &str,
    batch: u64,
    deadline: Duration,
) -> anyhow::Result<CapturedBatch> {
    let started = std::time::Instant::now();
    loop {
        let fri = peek_json(format!("{prover_api_url}/prover-jobs/v1/FRI/{batch}/peek")).await?;
        let zisk = peek_json(format!("{prover_api_url}/prover-jobs/v1/ZiSK/{batch}/peek")).await?;
        if let (Some(fri), Some(zisk)) = (fri, zisk) {
            let b64 = base64::engine::general_purpose::STANDARD;
            let witness_bytes = b64.decode(
                fri.get("prover_input")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            )?;
            return Ok(CapturedBatch {
                vk_hash: fri
                    .get("vk_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                airbender_witness: witness_bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect(),
                zisk_input: b64.decode(
                    zisk.get("zisk_data")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default(),
                )?,
            });
        }
        anyhow::ensure!(
            started.elapsed() < deadline,
            "batch {batch} did not expose both prover inputs within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The Airbender-formula public input derived from the ZiSK guest's
/// re-execution: keccak(state_before ‖ state_after ‖ batch_output), as LE
/// u32 register values.
fn guest_expected_registers(zisk_input: &[u8]) -> anyhow::Result<[u32; 8]> {
    // `wire` is the single source of truth for the server-to-guest format.
    let input: zksync_os_zisk_lib::types::BatchInput = zksync_os_zisk_lib::wire::decode(zisk_input)
        .map_err(|e| anyhow::anyhow!("failed to decode the ZiSK batch input: {e}"))?;
    let (_output, _commitment, state_before, state_after, batch_hash) =
        zksync_os_zisk_lib::executor::execute_and_commit_debug(&input);
    let pi: B256 = keccak256([state_before.0, state_after.0, batch_hash.0].concat());
    Ok(pi
        .0
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap())
}

#[test_log::test(tokio::test)]
async fn witness_consistency_across_lanes() -> anyhow::Result<()> {
    // The lane is built on prover input generation, which the `no-pig`
    // profile turns off.
    if std::env::var("NEXTEST_PROFILE").as_deref() == Ok("no-pig") {
        tracing::warn!("no-pig profile — skipping witness consistency test");
        return Ok(());
    }

    let env = CURRENT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    // This test peeks the /FRI and /ZiSK routes, so it turns the lane on.
    enable_second_proof_system(&mut config);
    // Both in-process fake provers off: the harness keeps the prover API
    // bound (it disables the API when both fakes run), and the captured jobs
    // stay in the job map instead of being consumed.
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
    let tester = env.launch(config).await?;
    let prover_api_url = tester
        .prover_api_url()
        .expect("prover API must be bound when the fake provers are off");

    // Deterministic batch boundaries: exactly `BATCHES` sealed batches.
    tester.drive_to_exact_sealed_batches(BATCHES).await?;

    for batch in 1..=BATCHES {
        let captured = capture_batch(&prover_api_url, batch, Duration::from_secs(120)).await?;

        // A v31 chain proves with V7, so one app binary covers every batch.
        anyhow::ensure!(
            captured.vk_hash == ProvingVersion::V7.vk_hash(),
            "batch {batch}: expected the v31 (V7) proving version, got vk hash {}",
            captured.vk_hash
        );
        let registers = execution_utils_prev::run_verifier_binary(
            zksync_os_multivm::apps::v7::MULTIBLOCK_BATCH,
            captured.airbender_witness.clone(),
        )
        .ok_or_else(|| anyhow::anyhow!("batch {batch}: simulation did not reach the exit point"))?;

        // The ZiSK guest's independent view of the same batch.
        let expected = guest_expected_registers(&captured.zisk_input)?;

        anyhow::ensure!(
            registers[..8] == expected,
            "batch {batch}: Airbender-simulated public input {:?} != guest-derived {:?}",
            &registers[..8],
            expected,
        );
        tracing::info!(batch, "lanes agree");
    }

    tracing::info!(batches = BATCHES, "witness consistency verified");
    Ok(())
}
