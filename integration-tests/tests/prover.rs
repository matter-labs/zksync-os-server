#![cfg(feature = "prover-tests")]

use zksync_os_integration_tests::{CURRENT_TO_L1, TestEnvironment, V30_TO_L1, test_multisetup};

#[cfg(feature = "gpu-prover-tests")]
mod real_provers {
    use alloy::network::TransactionBuilder;
    use alloy::primitives::{Address, B256, U256};
    use alloy::providers::Provider;
    use alloy::rpc::types::TransactionRequest;
    use base64::Engine;
    use std::time::Duration;
    use zksync_os_integration_tests::l1_helpers::{assert_settled_with_multiproof, fetch_l1_state};
    use zksync_os_integration_tests::{
        AirbenderMode, CURRENT_TO_L1, CURRENT_TO_MULTIPROVER_L1, Tester, assert_zisk_lane_accepted,
        run_zisk_gpu_prover, spawn_airbender_prover, spawn_airbender_prover_with_mode,
        wait_for_zisk_aggregation_ranges, zisk_gpu_artifacts_available,
    };
    use zksync_os_server::config::ZiskVkConfigEntry;
    use zksync_os_server::default_protocol_version::PROTOCOL_VERSION_V31_0;
    use zksync_os_types::ProtocolSemanticVersion;

    /// How long to allow the Airbender lane to prove every FRI of a range and
    /// pick the range's SNARK job with real (GPU) proving in the loop: prover
    /// warmup (one-time SNARK precomputations — observed up to ~25 minutes) +
    /// commit + FRI proving.
    const AIRBENDER_SNARK_RANGE_TIMEOUT: Duration = Duration::from_secs(3600);

    /// How long to allow the range to settle on L1 once the ZiSK range proof is
    /// parked: the abandoned SNARK job's lease has to expire
    /// (`snark_job_timeout`), the second Airbender run pays the SNARK warmup
    /// again, and the multi-proof then commits, proves and executes.
    const MULTI_PROOF_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(3600);

    /// Kills the wrapped prover service when dropped, so a failing test does
    /// not leak a GPU-holding orphan process.
    struct KillOnDrop(tokio::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.start_kill();
        }
    }

    /// Settle the aggregation range on L1 as a multi-proof.
    ///
    /// Under `multi_proof_verifier` the server rejected the Airbender range
    /// SNARK while the ZiSK range proof was missing and left the job
    /// re-offerable. The ZiSK proof is parked now, so a second Airbender run
    /// picks that job up, the server composes the type-5 payload, and the
    /// chain's MultiProofVerifier checks both proofs together. Only the
    /// multiprover L1 accepts that payload, so this is the assertion that both
    /// lanes agreed on the same state transition.
    ///
    /// Executed on L1 implies proved on L1, so the wait covers the whole
    /// settlement pipeline.
    async fn settle_multi_proof_on_l1(
        tester: &Tester,
        prover_api_urls: &[String],
        batches: u64,
    ) -> anyhow::Result<()> {
        // SnarkOnly: every FRI of the range is already proven, and the FRI
        // loop only breaks on a count it would never reach — the service must
        // go straight to the SNARK pick (the first settle attempt deadlocked
        // exactly here, polling FRI work forever).
        let mut airbender = KillOnDrop(
            spawn_airbender_prover_with_mode(
                tester,
                PROTOCOL_VERSION_V31_0,
                prover_api_urls,
                1000,
                AirbenderMode::SnarkOnly,
            )
            .await,
        );
        let deadline = std::time::Instant::now() + MULTI_PROOF_SETTLEMENT_TIMEOUT;
        loop {
            let state = fetch_l1_state(tester).await?;
            if state.last_executed_batch >= batches {
                tracing::info!(batches, "the multi-proof settled on L1");
                airbender.0.kill().await.ok();
                // Executed on L1 is not enough: the fixture's testnet verifier
                // also accepts empty and mock proofs, so assert on what was
                // actually submitted.
                assert_settled_with_multiproof(tester).await?;
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "the multi-proof did not settle within {MULTI_PROOF_SETTLEMENT_TIMEOUT:?}: L1 \
                 holds {} committed, {} proved and {} executed batches, expected {batches} \
                 executed",
                state.last_committed_batch,
                state.last_proved_batch,
                state.last_executed_batch
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Build the version-keyed ZiSK VK config from the recorded
    /// reproducible-build keys, arming the per-batch drift tripwire for each
    /// protocol version the test proves. An entry pins both keys, so both
    /// `ZISK_PROGRAM_VK` and `ZISK_VADCOP_VK` must be set to arm it; otherwise
    /// the list stays empty and the reported VKs are only logged — which the
    /// required mode refuses: `config.validate()` never runs in these tests,
    /// so nothing else notices a lane running with its VK tripwire disarmed.
    fn zisk_vks_from_env(
        versions: &[ProtocolSemanticVersion],
    ) -> anyhow::Result<Vec<ZiskVkConfigEntry>> {
        let (Ok(program), Ok(vadcop)) = (
            std::env::var("ZISK_PROGRAM_VK"),
            std::env::var("ZISK_VADCOP_VK"),
        ) else {
            anyhow::ensure!(
                !zksync_os_integration_tests::zisk_gpu_tests_required(),
                "ZISK_GPU_TESTS_REQUIRED is set but ZISK_PROGRAM_VK or ZISK_VADCOP_VK is not: \
                 the run would prove the lane with its VK drift tripwire disarmed"
            );
            return Ok(Vec::new());
        };
        let program_vk: B256 = program.parse()?;
        let vadcop_vk: B256 = vadcop.parse()?;
        Ok(versions
            .iter()
            .map(|v| ZiskVkConfigEntry {
                protocol_version: v.clone(),
                program_vk,
                vadcop_vk,
            })
            .collect())
    }

    /// Drive one transfer per batch and return each sealed batch's ZiSK input,
    /// peeked from the prover API. Batch sealing and ZiSK job creation are
    /// upstream of proving, so this works with no prover running.
    async fn seal_batches_and_peek_inputs(
        tester: &Tester,
        prover_api_url: &str,
        batches: u64,
    ) -> anyhow::Result<Vec<Vec<u8>>> {
        let recipient: Address = "0xdead000000000000000000000000000000000001".parse()?;
        let mut inputs = Vec::with_capacity(batches as usize);
        for batch in 1..=batches {
            tester
                .l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(recipient)
                        .with_value(U256::from(batch)),
                )
                .await?
                .get_receipt()
                .await?;

            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            let input = loop {
                let response = reqwest::Client::new()
                    .get(format!("{prover_api_url}/prover-jobs/v1/ZiSK/{batch}/peek"))
                    .send()
                    .await?;
                let status = response.status();
                if status == reqwest::StatusCode::OK {
                    let payload: serde_json::Value = response.json().await?;
                    let data = payload
                        .get("zisk_data")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("batch {batch}: no zisk_data"))?;
                    break base64::engine::general_purpose::STANDARD.decode(data)?;
                }
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "batch {batch} ZiSK job did not appear within 120s (status {status})"
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
            };
            inputs.push(input);
        }
        Ok(inputs)
    }

    /// Dump a fresh four-batch session's ZiSK `BatchInput`s for fixture
    /// regeneration (runbook step 6): the zisk-fixture-session workflow runs
    /// this on a CPU runner, then proves and aggregates the dumped inputs on a
    /// GPU runner to re-derive the real-proof fixtures and the binding vector
    /// after a guest rotation. Sealing needs no prover and no GPU. Gated on
    /// `ZISK_SESSION_DUMP_DIR` so ordinary prover-test runs skip it; the skip
    /// is loud but harmless — this test asserts nothing, it produces inputs.
    #[test_log::test(tokio::test)]
    async fn zisk_dump_session_inputs() -> anyhow::Result<()> {
        let Ok(dir) = std::env::var("ZISK_SESSION_DUMP_DIR") else {
            eprintln!(
                "NOTE: zisk_dump_session_inputs SKIPPED: ZISK_SESSION_DUMP_DIR is not set. \
                 This test only feeds the fixture-regeneration workflow."
            );
            return Ok(());
        };
        const BATCHES: u64 = 4;

        let env = CURRENT_TO_MULTIPROVER_L1.environment().await?;
        let mut config = env.default_config().await?;
        // Fakes off: a fake FRI/SNARK pass would finalize and discard the
        // sealed batches' ZiSK jobs before the inputs are peeked.
        config.prover_api_config.fake_fri_provers.enabled = false;
        config.prover_api_config.fake_snark_provers.enabled = false;
        zksync_os_integration_tests::test_config::enable_second_proof_system(&mut config);
        let tester = env.launch_without_provers(config).await?;
        let prover_api_url = tester
            .prover_api_url()
            .expect("prover API must be bound for prover tests");

        let inputs = seal_batches_and_peek_inputs(&tester, &prover_api_url, BATCHES).await?;
        std::fs::create_dir_all(&dir)?;
        for (i, input) in inputs.iter().enumerate() {
            // ziskemu framing, as the prover daemon's write_input and the
            // guest repo's dump_to_batchinput produce it: [len u64 LE][wire
            // bytes][zero pad to 8]. cargo-zisk rejects unframed inputs
            // (EmuContext requires a multiple of 8).
            let mut framed = Vec::with_capacity(8 + input.len() + 8);
            framed.extend_from_slice(&(input.len() as u64).to_le_bytes());
            framed.extend_from_slice(input);
            let padding = (8 - ((8 + input.len()) % 8)) % 8;
            framed.extend(std::iter::repeat_n(0u8, padding));
            assert_eq!(framed.len() % 8, 0);
            let path = format!("{dir}/batch-{}.bin", i + 1);
            std::fs::write(&path, &framed)?;
            eprintln!("wrote {} bytes to {path}", framed.len());
        }
        Ok(())
    }

    /// The two-lane flow with REAL provers on the multiprover v31 chain, with
    /// the MultiProofVerifier armed (`multi_proof_verifier`): the Airbender
    /// range SNARK settles only together with the aggregated ZiSK proof of the
    /// same range, here a two-batch range (`two_lane_multibatch_e2e` covers a
    /// four-batch one).
    ///
    /// Both lanes share one GPU — shivini statically claims most of the VRAM
    /// per process — so they run one after the other, and the order follows
    /// the Required-mode commit gate: a batch's data commits only once BOTH
    /// proof systems proved it, and the SNARK job that registers the
    /// aggregation range only exists after the commit. So ZiSK proves the
    /// batch first; Airbender then proves the FRI, the gate releases, the
    /// batch commits, and the SNARK pick registers the range — its own
    /// submission is rejected while the ZiSK range proof is missing, which is
    /// what keeps the range alive while the GPU moves back to the ZiSK daemon
    /// for the aggregation. A final Airbender run settles the range on L1 as
    /// one multi-proof.
    ///
    /// Two batches in ONE range, deliberately: chain boot itself seals two
    /// batches (the upgrade-tx batch and the first block batch), so a
    /// single-batch chain does not exist, and with `max_fris_per_snark = 1`
    /// every extra batch would need its own full ZiSK-Airbender-ZiSK cycle.
    /// This is the minimal single-cycle settle; sequential single-batch
    /// (width-1) ranges are real coverage, but they belong in their own test,
    /// not hidden inside this one.
    #[test_log::test(tokio::test)]
    async fn two_lane_single_range_e2e() -> anyhow::Result<()> {
        const BATCHES: u64 = 2;
        // The daemon proves each batch to a `vadcop_final` stream and collapses
        // the formed range into one aggregated proof.
        const RANGE_PROOFS: u64 = 1;
        const MAX_FRIS_PER_SNARK: usize = 2;

        if !zisk_gpu_artifacts_available("two_lane_single_range_e2e") {
            return Ok(());
        }

        let env = CURRENT_TO_MULTIPROVER_L1.environment().await?;
        let mut config = env.default_config().await?;
        config.prover_api_config.fake_fri_provers.enabled = false;
        config.prover_api_config.fake_snark_provers.enabled = false;
        // Real-prover path: turn the ZiSK lane on.
        zksync_os_integration_tests::test_config::enable_second_proof_system(&mut config);
        // Both boot batches fit one SNARK range, so one cycle settles them.
        config.prover_api_config.max_fris_per_snark = MAX_FRIS_PER_SNARK;
        // The production shape: an Airbender range settles only as a
        // MultiProof, so the ZiSK range proof gates it.
        config.prover_input_generator_config.multi_proof_verifier = true;
        // Every batch proves under the chain's single protocol version.
        config.prover_api_config.zisk_vks =
            zisk_vks_from_env(&[ProtocolSemanticVersion::new(0, 31, 0)])?;
        let tester = env.launch_without_provers(config).await?;
        let prover_api_url = tester
            .prover_api_url()
            .expect("prover API must be bound for prover tests");
        let urls = vec![prover_api_url.clone()];

        // Produce EXACTLY `BATCHES` batches. Their ZiSK per-batch jobs are
        // created at seal, so the daemon has work before any prover runs.
        tester.drive_to_exact_sealed_batches(BATCHES).await?;

        // ZiSK first: the Required-mode gate holds the data commit until the
        // second proof exists, and the SNARK job (whose pick registers the
        // aggregation range) is created downstream of the commit. Airbender
        // first would deadlock: no commit without ZiSK, no SNARK pick without
        // commit, and this test starts ZiSK only after the pick.
        run_zisk_gpu_prover(&prover_api_url, BATCHES as usize).await;
        assert_zisk_lane_accepted(&prover_api_url, BATCHES, 0).await?;

        // Airbender: FRI proofs release the gate, the batch commits, and the
        // SNARK pick registers the range with the ZiSK aggregation stage. Huge
        // iteration budget: the service is killed once the range registers —
        // its own SNARK submission is rejected while the ZiSK range proof is
        // missing, which keeps the job re-offerable for the settle stage.
        let mut airbender = KillOnDrop(
            spawn_airbender_prover(
                &tester,
                PROTOCOL_VERSION_V31_0,
                &urls,
                1000,
                MAX_FRIS_PER_SNARK,
            )
            .await,
        );
        wait_for_zisk_aggregation_ranges(
            &prover_api_url,
            RANGE_PROOFS,
            AIRBENDER_SNARK_RANGE_TIMEOUT,
        )
        .await?;

        // The range is registered — free the GPU for the ZiSK aggregation.
        airbender.0.kill().await.ok();
        tracing::info!("Airbender SNARK range registered — aggregating on the ZiSK lane");

        run_zisk_gpu_prover(&prover_api_url, RANGE_PROOFS as usize).await;
        assert_zisk_lane_accepted(&prover_api_url, BATCHES, RANGE_PROOFS).await?;

        settle_multi_proof_on_l1(&tester, &urls, BATCHES).await?;

        Ok(())
    }

    /// ZiSK lane in isolation: boot the chain with all provers off, seal a
    /// couple of batches, and run the real GPU daemon over them — pickup,
    /// prove, proof-file parse, submission, and the server's commitment +
    /// programVK validation, without the Airbender flow. Finality never
    /// advances here (nothing serves FRI jobs), which is fine: batch sealing
    /// and ZiSK job creation are upstream of proving. No Airbender SNARK
    /// registers a range, so the daemon proves per-batch streams only.
    #[test_log::test(tokio::test)]
    async fn zisk_lane_on_sealed_batches() -> anyhow::Result<()> {
        const BATCHES: u64 = 2;

        if !zisk_gpu_artifacts_available("zisk_lane_on_sealed_batches") {
            return Ok(());
        }

        let env = CURRENT_TO_L1.environment().await?;
        let mut config = env.default_config().await?;
        // Fakes stay off: a fake FRI/SNARK pass would finalize and discard
        // the sealed batches' ZiSK jobs before the daemon picks them.
        config.prover_api_config.fake_fri_provers.enabled = false;
        config.prover_api_config.fake_snark_provers.enabled = false;
        // Real-prover path: turn the ZiSK lane on (`max_fris_per_snark = 1`).
        zksync_os_integration_tests::test_config::enable_second_proof_system(&mut config);
        // Every batch proves under the chain's single protocol version.
        config.prover_api_config.zisk_vks =
            zisk_vks_from_env(&[ProtocolSemanticVersion::new(0, 31, 0)])?;
        let tester = env.launch_without_provers(config).await?;
        let prover_api_url = tester
            .prover_api_url()
            .expect("prover API must be bound for prover tests");

        seal_batches_and_peek_inputs(&tester, &prover_api_url, BATCHES).await?;

        run_zisk_gpu_prover(&prover_api_url, BATCHES as usize).await;
        assert_zisk_lane_accepted(&prover_api_url, BATCHES, 0).await?;

        Ok(())
    }

    /// The two-lane multi-batch flow on the multiprover v31 chain: Airbender
    /// proves a 4-batch SNARK range (max_fris_per_snark = 4), then the ZiSK
    /// daemon proves each batch to a vadcop_final, aggregates the range in the
    /// aggregator guest, and submits one range proof — accepted only if its
    /// binding digest matches the server's recomputation over the same batch
    /// range (single GPU: lanes run one after the other, see
    /// `two_lane_per_batch_e2e`). Airbender then returns to settle the range on
    /// L1 as one multi-proof.
    ///
    /// The batch boundaries are produced by `drive_to_exact_sealed_batches`,
    /// which yields exactly one full aggregation range without a per-batch
    /// content limit (see its docs for why `tx_per_batch_limit = 1` is not a
    /// valid driver).
    ///
    /// Env: ZISK_AGG_ELF (aggregator guest ELF path) + the usual
    /// gpu-prover-tests set; ZISK_PROGRAM_VK + ZISK_VADCOP_VK (per-batch lane)
    /// and ZISK_AGG_PROGRAM_VK (aggregator) arm the per-lane VK tripwires.
    #[test_log::test(tokio::test)]
    async fn two_lane_multibatch_e2e() -> anyhow::Result<()> {
        const BATCHES: u64 = 4;
        // All four batches collapse into the single Airbender SNARK range.
        const RANGE_PROOFS: u64 = 1;
        // One SNARK covers the whole range.
        const MAX_FRIS_PER_SNARK: usize = BATCHES as usize;

        if !zisk_gpu_artifacts_available("two_lane_multibatch_e2e") {
            return Ok(());
        }

        let env = CURRENT_TO_MULTIPROVER_L1.environment().await?;
        let mut config = env.default_config().await?;
        config.prover_api_config.fake_fri_provers.enabled = false;
        config.prover_api_config.fake_snark_provers.enabled = false;
        // Real-prover path: turn the ZiSK lane on, then widen the aggregation
        // range to one full SNARK group.
        zksync_os_integration_tests::test_config::enable_second_proof_system(&mut config);
        config.prover_api_config.max_fris_per_snark = MAX_FRIS_PER_SNARK;
        // The production shape: an Airbender range settles only as a
        // MultiProof, so the ZiSK range proof gates it.
        config.prover_input_generator_config.multi_proof_verifier = true;
        // Every batch proves under the chain's single protocol version.
        config.prover_api_config.zisk_vks =
            zisk_vks_from_env(&[ProtocolSemanticVersion::new(0, 31, 0)])?;
        if let Ok(vk) = std::env::var("ZISK_AGG_PROGRAM_VK") {
            config.prover_api_config.zisk_aggregation.program_vk = Some(vk.parse()?);
        } else {
            anyhow::ensure!(
                !zksync_os_integration_tests::zisk_gpu_tests_required(),
                "ZISK_GPU_TESTS_REQUIRED is set but ZISK_AGG_PROGRAM_VK is not: the aggregator \
                 lane would run with its VK tripwire disarmed"
            );
        }
        let tester = env.launch_without_provers(config).await?;
        let prover_api_url = tester
            .prover_api_url()
            .expect("prover API must be bound for prover tests");
        let urls = vec![prover_api_url.clone()];

        // Airbender first (single GPU: shivini claims all VRAM). One SNARK
        // covers all four batches, so the service must keep proving FRIs
        // until it holds all four — with a smaller cap it would stop after
        // the first FRI while the server (in aggregated mode) withholds
        // SNARK work until a full range is ready, deadlocking the run.
        let mut airbender = KillOnDrop(
            spawn_airbender_prover(
                &tester,
                PROTOCOL_VERSION_V31_0,
                &urls,
                1000,
                MAX_FRIS_PER_SNARK,
            )
            .await,
        );

        // Produce EXACTLY `BATCHES` batches — one full aggregation range,
        // nothing stranded past it — deterministically and without a
        // per-batch content limit (see `drive_to_exact_sealed_batches`).
        tester.drive_to_exact_sealed_batches(BATCHES).await?;

        // The SNARK job covers all four batches, so its pick registers the
        // four-batch aggregation range.
        wait_for_zisk_aggregation_ranges(
            &prover_api_url,
            RANGE_PROOFS,
            AIRBENDER_SNARK_RANGE_TIMEOUT,
        )
        .await?;
        airbender.0.kill().await.ok();
        tracing::info!("4-batch Airbender SNARK range registered — starting aggregated ZiSK lane");

        // ZiSK aggregated lane: 4 per-batch vadcop_final submissions + 1 range
        // proof. The range job becomes available once all four inputs are
        // buffered.
        run_zisk_gpu_prover(&prover_api_url, (BATCHES + RANGE_PROOFS) as usize).await;
        assert_zisk_lane_accepted(&prover_api_url, BATCHES, RANGE_PROOFS).await?;

        settle_multi_proof_on_l1(&tester, &urls, BATCHES).await?;

        Ok(())
    }

    /// Capture N sealed batches' ZiSK `BatchInput`s to disk, without proving.
    /// Feeds offline proving flows (e.g. minting `vadcop_final` proofs for
    /// range aggregation) with a coherent sequence of real chain batches.
    /// Env: `CAPTURE_BATCHES` (default 4), `CAPTURE_DIR` (default
    /// /tmp/zisk-batch-inputs). Writes `batch-N.bin` (raw bincode) and
    /// `batch-N.input.bin` (cargo-zisk framing: [len u64 LE][data][pad→8]).
    #[test_log::test(tokio::test)]
    async fn capture_sealed_batch_inputs() -> anyhow::Result<()> {
        if !zisk_gpu_artifacts_available("capture_sealed_batch_inputs") {
            return Ok(());
        }

        let batches: u64 = std::env::var("CAPTURE_BATCHES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let dir =
            std::env::var("CAPTURE_DIR").unwrap_or_else(|_| "/tmp/zisk-batch-inputs".to_string());
        std::fs::create_dir_all(&dir)?;

        let env = CURRENT_TO_L1.environment().await?;
        let mut config = env.default_config().await?;
        config.prover_api_config.fake_fri_provers.enabled = false;
        config.prover_api_config.fake_snark_provers.enabled = false;
        // Real-prover path: turn the ZiSK lane on (`max_fris_per_snark = 1`).
        zksync_os_integration_tests::test_config::enable_second_proof_system(&mut config);
        let tester = env.launch_without_provers(config).await?;
        let prover_api_url = tester
            .prover_api_url()
            .expect("prover API must be bound for prover tests");

        let inputs = seal_batches_and_peek_inputs(&tester, &prover_api_url, batches).await?;
        for (index, zisk_data) in inputs.iter().enumerate() {
            let batch = index as u64 + 1;
            std::fs::write(format!("{dir}/batch-{batch}.bin"), zisk_data)?;
            let mut framed = (zisk_data.len() as u64).to_le_bytes().to_vec();
            framed.extend_from_slice(zisk_data);
            while !framed.len().is_multiple_of(8) {
                framed.push(0);
            }
            std::fs::write(format!("{dir}/batch-{batch}.input.bin"), &framed)?;
            tracing::info!(batch, bytes = zisk_data.len(), "captured ZiSK batch input");
        }

        tracing::info!(batches, dir = %dir, "all batch inputs captured");
        Ok(())
    }
}

#[test_multisetup([CURRENT_TO_L1, V30_TO_L1])]
async fn prover(env: TestEnvironment) -> anyhow::Result<()> {
    // Test that prover can successfully prove at least one batch
    let mut config = env.default_config().await?;
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
    config.prover_input_generator_config.logging_enabled = true;
    let tester = env.launch(config).await?;

    // Test environment comes with some L1 transactions by default, so one batch should be provable
    // without any new transactions inside the test.
    tester.prover_tester.wait_for_batch_proven(1).await?;

    // todo: consider expanding this test to prove multiple batches on top of the first batch
    //       also to test L2 transactions are provable too

    Ok(())
}
