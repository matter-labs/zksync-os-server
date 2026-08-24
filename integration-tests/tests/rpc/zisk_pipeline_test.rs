//! End-to-end tests of the ZiSK second-proof-system input pipeline.
//!
//! The input-pipeline tests boot a v31 chain directly (`CURRENT_TO_L1`), drive
//! real transactions through the node, then fetch the server-assembled ZiSK
//! `BatchInput` from the prover API's `/ZiSK/{batch}/peek` endpoint — the exact
//! bytes an external ZiSK prover would receive — and re-execute it with the
//! ZiSK REVM executor, checking the execution results against the RPC receipts.
//!
//! Those tests do not finalize batches on L1 (that would need SNARK proving),
//! so blocks are located in batches by re-executing the peeked inputs and
//! matching block numbers, not via the batch-by-block RPC (which only serves
//! finalized batches). Under the `no-pig` profile the tests that need prover
//! input generation skip themselves.
//!
//! `two_lane_batch_boundaries_deterministic` is the exception: it boots the
//! multiprover chain and settles over the fake-proving path, which is the
//! GPU-free half of the two-lane E2E.

use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use base64::Engine;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::l1_helpers::wait_for_l1_state;
use zksync_os_integration_tests::test_config::{
    enable_second_proof_system, make_commit_only_config, make_full_pipeline_config,
};
use zksync_os_integration_tests::{CURRENT_TO_L1, CURRENT_TO_MULTIPROVER_L1};
use zksync_os_zisk_lib::executor;
use zksync_os_zisk_lib::types::BatchOutput;

/// The `no-pig` profile turns prover input generation off, and the ZiSK lane
/// is built on top of it. Tests that drive the lane honor that intent and skip.
fn no_pig_profile() -> bool {
    std::env::var("NEXTEST_PROFILE").as_deref() == Ok("no-pig")
}

/// Equivalence teeth for the lane tests: the REVM consistency checker reverts
/// on any native-vs-REVM divergence (armed for the whole suite in
/// `build_node_config`), and every sealed batch's ZiSK input is re-executed
/// in-process with the guest executor and checked against the expected batch
/// public input. Both fail the node, and therefore the test, loudly.
fn enable_shadow_execution(config: &mut zksync_os_server::config::Config) {
    config.prover_input_generator_config.zisk_shadow_execution = true;
    config
        .prover_input_generator_config
        .halt_on_zisk_commitment_mismatch = true;
}

#[derive(serde::Deserialize)]
struct ZiskBatchDataPayload {
    batch_number: u64,
    #[allow(dead_code)]
    vk_hash: String,
    zisk_data: String,
}

/// Single-shot peek: the batch's ZiSK input if it is currently available.
async fn peek_zisk_data_once(
    client: &reqwest::Client,
    prover_api_url: &str,
    batch_number: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    let url = format!("{prover_api_url}/prover-jobs/v1/ZiSK/{batch_number}/peek");
    let response = client.get(&url).send().await?;
    // Only 200 carries a payload; 204 means the batch is not in the FRI job
    // map (yet), 404 that its ProverInput carries no ZiSK data.
    if response.status() != reqwest::StatusCode::OK {
        return Ok(None);
    }
    let payload: ZiskBatchDataPayload = response.json().await?;
    anyhow::ensure!(
        payload.batch_number == batch_number,
        "batch number mismatch"
    );
    Ok(Some(
        base64::engine::general_purpose::STANDARD.decode(payload.zisk_data)?,
    ))
}

/// Poll the prover API until the batch's ZiSK data is available.
async fn peek_zisk_data(prover_api_url: &str, batch_number: u64) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::new();
    for _ in 0..240 {
        if let Some(bytes) = peek_zisk_data_once(&client, prover_api_url, batch_number).await? {
            return Ok(bytes);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for ZiSK data of batch {batch_number}")
}

/// Scan peekable batches until one's re-executed input contains
/// `block_number`; returns its batch number, execution output and commitment.
async fn wait_input_containing_block(
    prover_api_url: &str,
    max_batch: u64,
    block_number: u64,
) -> anyhow::Result<(u64, BatchOutput, B256)> {
    let client = reqwest::Client::new();
    for _ in 0..240 {
        for batch_number in 2..=max_batch {
            let Some(bytes) = peek_zisk_data_once(&client, prover_api_url, batch_number).await?
            else {
                continue;
            };
            let (output, commitment) =
                executor::execute_and_commit_from_bincode(&bytes).map_err(|e| {
                    anyhow::anyhow!("ZiSK executor failed for batch {batch_number}: {e}")
                })?;
            if output
                .block_results
                .iter()
                .any(|br| br.block_number == block_number)
            {
                return Ok((batch_number, output, commitment));
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for a ZiSK batch input containing block {block_number}")
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zisk_pipeline_e2e() -> anyhow::Result<()> {
    if no_pig_profile() {
        tracing::warn!("no-pig profile — skipping ZiSK pipeline test");
        return Ok(());
    }

    let env = CURRENT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    // This test peeks the /ZiSK routes, so it turns the lane on. The general
    // suite keeps it off (see `build_node_config`).
    enable_second_proof_system(&mut config);
    // Both in-process fake provers off: the harness keeps the prover API
    // bound (it disables the API when both fakes run), and FRI jobs stay in
    // the job map so /ZiSK/{batch}/peek can serve them.
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
    enable_shadow_execution(&mut config);
    let tester = env.launch(config).await?;

    let prover_api_url = tester
        .prover_api_url()
        .expect("prover API must be bound when the fake provers are off");

    let recipient: Address = "0xdead000000000000000000000000000000000001".parse()?;

    // 1. Drive real traffic: an ETH transfer and a contract deployment.
    let transfer_receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(recipient)
                .with_value(U256::from(1_000_000_000_000_000_000u128)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // Minimal deployment: contract with code `0x00` (STOP).
    // Init code: PUSH1 0x01 PUSH1 0x0c PUSH1 0x00 CODECOPY PUSH1 0x01 PUSH1 0x00 RETURN
    let init_code = alloy::hex::decode("6001600c60003960016000f300")?;
    let deploy_receipt = tester
        .l2_provider
        .send_transaction(TransactionRequest::default().with_deploy_code(init_code))
        .await?
        .expect_successful_receipt()
        .await?;

    // 2. For each driven transaction, find the server-assembled BatchInput
    //    whose ZiSK re-execution contains its block, and cross-check the
    //    execution against the RPC receipt. (No prover consumes jobs in this
    //    config, so every generated batch input stays peekable.)
    for receipt in [&transfer_receipt, &deploy_receipt] {
        let block_number = receipt.block_number().expect("receipt has block number");
        let (batch_number, output, commitment) =
            wait_input_containing_block(&prover_api_url, 8, block_number).await?;

        assert_ne!(
            commitment,
            B256::ZERO,
            "batch commitment must be non-trivial"
        );
        let block_result = output
            .block_results
            .iter()
            .find(|br| br.block_number == block_number)
            .expect("scan matched this block");
        let tx_index = receipt.transaction_index().expect("receipt has tx index") as usize;
        let tx_result = &block_result.tx_results[tx_index];
        assert!(tx_result.success, "tx must succeed in ZiSK re-execution");
        assert_eq!(
            tx_result.gas_used,
            receipt.gas_used(),
            "gas mismatch for tx {} in block {block_number}",
            receipt.transaction_hash()
        );

        tracing::info!(
            batch_number,
            block_number,
            %commitment,
            "ZiSK executor reproduced the batch"
        );
    }

    // 3. L1→L2 deposit: a priority transaction reaches the guest via the
    //    TxAuth::L1 wire path (mint, bootloader result log, priority-ops
    //    hash). Cross-check its re-execution the same way.
    let deposit_l2_hash = tester
        .deposit_l1_to_l2(recipient, U256::from(1_000u64))
        .await?;
    let deposit_receipt = alloy::providers::PendingTransactionBuilder::new(
        tester.l2_zk_provider.root().clone(),
        deposit_l2_hash,
    )
    .expect_successful_receipt()
    .await?;
    let block_number = deposit_receipt
        .block_number
        .expect("deposit receipt has block number");
    let (batch_number, output, _) =
        wait_input_containing_block(&prover_api_url, 8, block_number).await?;
    let block_result = output
        .block_results
        .iter()
        .find(|br| br.block_number == block_number)
        .expect("scan matched this block");
    let tx_index = deposit_receipt
        .transaction_index
        .expect("deposit receipt has tx index") as usize;
    let tx_result = &block_result.tx_results[tx_index];
    assert!(
        tx_result.success,
        "deposit must succeed in ZiSK re-execution"
    );
    assert_eq!(
        tx_result.gas_used, deposit_receipt.gas_used,
        "gas mismatch for deposit {deposit_l2_hash} in block {block_number}"
    );
    tracing::info!(
        batch_number,
        block_number,
        "ZiSK executor reproduced the deposit's batch"
    );

    // 4. Batch 1 contains the genesis upgrade block and its mass
    //    force-deployments (the upgrade-batch fidelity case: every
    //    force-deployed account's code-derived property fields are recomputed
    //    and asserted by the executor).
    let zisk_bytes = peek_zisk_data(&prover_api_url, 1).await?;
    let (output, commitment) = executor::execute_and_commit_from_bincode(&zisk_bytes)
        .map_err(|e| anyhow::anyhow!("ZiSK executor failed for batch 1: {e}"))?;
    assert_ne!(
        commitment,
        B256::ZERO,
        "batch commitment must be non-trivial"
    );
    assert!(
        !output.block_results.is_empty(),
        "batch 1 produced no block results"
    );
    tracing::info!(%commitment, "ZiSK executor reproduced the genesis-upgrade batch");

    Ok(())
}

/// Crash-recovery property of the ZiSK lane: the lane holds batch inputs only
/// in memory (an active job or the job manager's parked backlog), so a restart
/// between batch commit and SNARK arrival loses the batch's `BatchInput`.
/// Committed-but-unproven batches must regain their ZiSK input on restart by
/// flowing through the batcher's catch-up re-seal and the prover input
/// generator again.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zisk_input_regenerated_after_restart() -> anyhow::Result<()> {
    if no_pig_profile() {
        tracing::warn!("no-pig profile — skipping ZiSK restart test");
        return Ok(());
    }

    let env = CURRENT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    // This test peeks the /ZiSK routes, so it turns the lane on.
    enable_second_proof_system(&mut config);
    // Fake FRI provers on, SNARK provers off: batches commit on L1 and then
    // stay unproven, exactly the window where a restart loses in-memory state.
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    // Drive a transaction so batch 2 has real content, then wait for it to be
    // committed (batch 1 is the genesis-upgrade batch; the tx lands in a
    // later batch).
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

    let committed_state = wait_for_l1_state(&tester, "a post-genesis committed batch", |state| {
        state.last_committed_batch >= 2
    })
    .await?;
    assert_eq!(
        committed_state.last_proved_batch, 0,
        "SNARK proving is disabled, so committed batches must stay unproven"
    );

    // Restart with FRI provers disabled: recreated batches enter the FRI job
    // map and stay there, so the regenerated ZiSK input remains peekable.
    let restarted = tester
        .restart_with_overrides(|config| {
            config.prover_api_config.fake_fri_provers.enabled = false;
        })
        .await?;
    let prover_api_url = restarted
        .prover_api_url()
        .expect("prover API must be bound when the fake SNARK provers are off");

    // Every committed-but-unproven batch — the genesis-upgrade batch included
    // — must reappear with a valid, executable BatchInput; committed batches
    // are recreated with their original numbers.
    for batch_number in 1..=committed_state.last_committed_batch {
        let zisk_bytes = peek_zisk_data(&prover_api_url, batch_number).await?;
        let (output, commitment) =
            executor::execute_and_commit_from_bincode(&zisk_bytes).map_err(|e| {
                anyhow::anyhow!("regenerated input for batch {batch_number} failed: {e}")
            })?;
        assert_ne!(
            commitment,
            B256::ZERO,
            "batch commitment must be non-trivial"
        );
        assert!(
            !output.block_results.is_empty(),
            "batch {batch_number} produced no block results"
        );
        tracing::info!(
            batch_number,
            %commitment,
            "ZiSK input regenerated and re-executed after restart"
        );
    }

    Ok(())
}

/// Header-hash fidelity across batch shapes: every peekable batch must
/// re-execute cleanly, and the run must include at least one multi-block
/// batch so the guest's intra-batch hash chaining (server-provided ring hash
/// of an earlier in-batch block vs the guest-recomputed one) actually runs.
/// Combined with the populated `block_header_hash` — which makes the guest
/// assert every block's recomputed header hash against the canonical one —
/// this pins the header format of the chain's execution version.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zisk_multiblock_batch_hashes() -> anyhow::Result<()> {
    if no_pig_profile() {
        tracing::warn!("no-pig profile — skipping ZiSK multi-block batch test");
        return Ok(());
    }

    let env = CURRENT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    // This test peeks the /ZiSK routes, so it turns the lane on.
    enable_second_proof_system(&mut config);
    // Both in-process fake provers off so the prover API stays bound and FRI
    // jobs remain peekable (see `zisk_pipeline_e2e`).
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
    // A wide batch window makes the multi-block property robust: blocks only
    // seal when they carry transactions, so the two receipt-waited transfers
    // per iteration land in two blocks that share a batch even when a loaded
    // machine stretches the receipt waits past the default 1s test window.
    config.batcher_config.batch_timeout = Duration::from_secs(5);
    enable_shadow_execution(&mut config);
    let tester = env.launch(config).await?;

    let prover_api_url = tester
        .prover_api_url()
        .expect("prover API must be bound when the fake provers are off");

    // Re-execute every peekable batch until a multi-block batch has been
    // observed. The guest asserts each block's recomputed header hash against
    // the canonical `block_header_hash` and cross-checks intra-batch ring
    // hashes, so any header drift panics here.
    //
    // Blocks only seal when they carry transactions, so each iteration sends
    // two receipt-waited transfers: two blocks that share a batch whenever
    // both land inside the batch window. A loaded machine can stretch the
    // receipt waits past any fixed window, so the loop keeps producing pairs
    // (deadline-bounded) instead of asserting on a fixed batch count.
    let recipient: Address = "0xdead000000000000000000000000000000000002".parse()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let client = reqwest::Client::new();
    let mut next_batch = 1u64;
    let mut multi_block_batches = 0usize;
    while multi_block_batches == 0 {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "no multi-block batch observed among the first {} batches within \
             the deadline — intra-batch hash verification was not exercised",
            next_batch - 1,
        );

        for _ in 0..2 {
            tester
                .l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(recipient)
                        .with_value(U256::from(1u64)),
                )
                .await?
                .expect_successful_receipt()
                .await?;
        }

        // Drain every batch whose input is already peekable; inputs stay
        // peekable (no prover consumes them), so the sequential scan is safe.
        while let Some(zisk_bytes) =
            peek_zisk_data_once(&client, &prover_api_url, next_batch).await?
        {
            let (output, commitment) = executor::execute_and_commit_from_bincode(&zisk_bytes)
                .map_err(|e| {
                    anyhow::anyhow!("ZiSK re-execution failed for batch {next_batch}: {e}")
                })?;
            assert_ne!(
                commitment,
                B256::ZERO,
                "batch commitment must be non-trivial"
            );
            if output.block_results.len() >= 2 {
                multi_block_batches += 1;
            }
            tracing::info!(
                batch_number = next_batch,
                blocks = output.block_results.len(),
                %commitment,
                "ZiSK executor reproduced the batch"
            );
            next_batch += 1;
        }
    }
    tracing::info!(
        multi_block_batches,
        batches_checked = next_batch - 1,
        "multi-block batch re-executed"
    );

    Ok(())
}

/// GPU-free half of the two-lane E2E: drives the batcher over the
/// fake-proving path with the same lane config and asserts the chain
/// rests at exactly `RANGE_SIZE` sealed batches — the boundary count
/// `prover::two_lane_multibatch_e2e` requires.
///
/// It boots the same chain as that run, so it is also the GPU-free check of
/// the multiprover L1 fixture: the baked state loads, the node initializes and
/// settles against it, and the chain's `MultiProofTestnetVerifier` still
/// accepts a mock proof.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn two_lane_batch_boundaries_deterministic() -> anyhow::Result<()> {
    // The aggregation range size the two-lane E2E drives against.
    const RANGE_SIZE: u64 = 4;

    let env = CURRENT_TO_MULTIPROVER_L1.environment().await?;
    let mut config = env.default_config().await?;
    // This is the GPU-free mirror of the two-lane E2E. That run enables the
    // ZiSK lane with `max_fris_per_snark = 1`, so this test does the same. The
    // batch boundaries then stay faithful to the real run.
    enable_second_proof_system(&mut config);
    // Drive over the fake-proving path: fake FRI + SNARK keep the node making
    // forward progress without a GPU, and the batcher (upstream of proving)
    // seals batches exactly as it does in the real two-lane run.
    make_full_pipeline_config(&mut config);
    let tester = env.launch(config).await?;

    // EXACTLY range_size sealed batches, deterministically. The driver's own
    // final check already fails loudly on any drift; re-assert here so the
    // exact count is an explicit property of this test.
    tester.drive_to_exact_sealed_batches(RANGE_SIZE).await?;
    let sealed = tester.batcher_progress().await?.last_sealed_batch;
    assert_eq!(
        sealed, RANGE_SIZE,
        "expected exactly {RANGE_SIZE} sealed batches"
    );

    // The boundary is stable, not merely momentarily hit: with driving
    // stopped there is no traffic, so no new block and no new batch may seal.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let sealed_after = tester.batcher_progress().await?.last_sealed_batch;
    assert_eq!(
        sealed_after, RANGE_SIZE,
        "an extra batch sealed after driving stopped"
    );

    // Every sealed batch settles on the multiprover L1 through the mock-proof
    // path, so the fixture serves the fake-prover suite as the default chain
    // does.
    wait_for_l1_state(&tester, "every sealed batch executed on L1", |state| {
        state.last_executed_batch >= RANGE_SIZE
    })
    .await?;

    tracing::info!(sealed, "two-lane batch boundaries are deterministic");
    Ok(())
}
