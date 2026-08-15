use super::snark_job_manager::SnarkJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zisk_prover_lane::BatchRange;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_batch_types::batcher_model::{RealSnarkProof, SnarkProof};
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// What the range stage does about a second proof system.
///
/// The mirror of [`SecondProofBatchStage`]: `Required` carries the aggregation
/// lane and the channel its parked proofs announce themselves on, so asking to
/// compose a multi-proof without them is not a value anyone can build.
pub enum SecondProofRangeStage {
    Disabled,
    /// Both lanes run; the range settles on the Airbender proof alone.
    Shadow {
        per_batch: Arc<zisk_prover_lane::ZiskJobManager>,
        aggregation: Arc<zisk_prover_lane::ZiskAggregationJobManager>,
    },
    /// The range settles only as a composed multi-proof.
    Required {
        per_batch: Arc<zisk_prover_lane::ZiskJobManager>,
        aggregation: Arc<zisk_prover_lane::ZiskAggregationJobManager>,
        /// Announced by the aggregation lane when a range proof parks.
        ready: mpsc::Receiver<BatchRange>,
    },
}

/// How many ready-range signals may queue before the aggregation lane stops
/// announcing. One in-flight range composes at a time, so a small buffer is
/// enough. Signals are coalesced wake tokens: the completed proof is stored
/// before `try_send`, and any queued token re-checks the one parked range.
pub const MAX_READY_RANGE_SIGNALS: usize = 8;

/// The range proving stage: a batch range leaves it once every proof system
/// that proves ranges has done so, and the resulting proof is handed to the L1
/// prove sender.
///
/// The stage is named for its level rather than for a proof system: today it
/// drives the Airbender SNARK lane, and a second proof system aggregating the
/// same ranges belongs here too.
///
/// This component:
/// - Receives batches with FRI proofs (after they are committed to L1)
/// - Forwards them to SnarkJobManager (which makes them available via HTTP API)
/// - Receives batches with proofs from SnarkJobManager (submitted via HTTP API or fake provers)
/// - Forwards the proof commands downstream to L1 proof sender
///
/// The SnarkJobManager itself is purely reactive (no run loop), accessed/driven by:
/// - HTTP server (provers call pick_next_job, submit_proof, etc.)
/// - Fake provers pool
pub struct RangeProvingPipelineStep {
    last_proved_batch_number: u64,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_commands_receiver: mpsc::Receiver<ProofCommand>,
    /// The second proof system's per-batch lane. The stage only ever tells it
    /// to drop state a fake range can never use.
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
    /// The second proof system's range lane. Present whenever it is enabled;
    /// composition happens here, in this stage, not inside either manager.
    zisk_aggregation_job_manager: Option<Arc<zisk_prover_lane::ZiskAggregationJobManager>>,
    /// Ranges whose aggregated ZiSK proof has just been parked.
    zisk_range_ready: Option<mpsc::Receiver<BatchRange>>,
    /// Whether a range may settle on Airbender alone.
    require_multi_proof: bool,
}

impl RangeProvingPipelineStep {
    pub fn new(
        max_fris_per_snark: usize,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<SnarkJobManager>) {
        Self::new_with_zisk(
            max_fris_per_snark,
            last_proved_batch_number,
            assignment_timeout,
            max_assigned_batch_range,
            SecondProofRangeStage::Disabled,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_zisk(
        max_fris_per_snark: usize,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
        second_proof: SecondProofRangeStage,
    ) -> (Self, Arc<SnarkJobManager>) {
        // Composing a multi-proof needs the aggregation lane and the channel
        // that announces its parked proofs; `Required` carries both, so
        // "compose" cannot be asked for without them.
        let (zisk_job_manager, zisk_aggregation_job_manager, require_multi_proof, zisk_range_ready) =
            match second_proof {
                SecondProofRangeStage::Disabled => (None, None, false, None),
                SecondProofRangeStage::Shadow {
                    per_batch,
                    aggregation,
                } => (Some(per_batch), Some(aggregation), false, None),
                SecondProofRangeStage::Required {
                    per_batch,
                    aggregation,
                    ready,
                } => (Some(per_batch), Some(aggregation), true, Some(ready)),
            };
        let (proof_commands_sender, proof_commands_receiver) = mpsc::channel::<ProofCommand>(1);

        let snark_job_manager = SnarkJobManager::new(
            proof_commands_sender,
            max_fris_per_snark,
            assignment_timeout,
            max_assigned_batch_range,
            zisk_aggregation_job_manager.is_some(),
        );

        if let Some(zisk_job_manager) = zisk_job_manager.clone() {
            // Periodic gauge refresh: queue/backlog ages must advance while the
            // lane is idle — a stalled pipeline is exactly what they alert on.
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(15));
                loop {
                    tick.tick().await;
                    zisk_job_manager.refresh_gauges().await;
                }
            });
            tracing::info!(
                aggregated = zisk_aggregation_job_manager.is_some(),
                require_multi_proof,
                "ZiSK lane enabled"
            );
        }

        let snark_job_manager = Arc::new(snark_job_manager);

        let result = Self {
            last_proved_batch_number,
            snark_job_manager: snark_job_manager.clone(),
            proof_commands_receiver,
            zisk_job_manager,
            zisk_aggregation_job_manager,
            zisk_range_ready,
            require_multi_proof,
        };

        (result, snark_job_manager)
    }
}

/// Outcome of trying to pair an Airbender range proof with its ZiSK half.
enum Composed {
    /// Ready to go downstream — either composed, or Airbender-only where that
    /// is what this chain settles.
    Ready(ProofCommand),
    /// The ZiSK half has not been proved yet; hold the Airbender proof.
    Waiting(ProofCommand),
}

/// The half of the stage that pairs the two lanes' range proofs.
struct RangeComposer {
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
    zisk_aggregation_job_manager: Option<Arc<zisk_prover_lane::ZiskAggregationJobManager>>,
    require_multi_proof: bool,
}

impl RangeComposer {
    /// Pair an Airbender range proof with the aggregated ZiSK proof of the same
    /// bounds, or hold it until that proof arrives.
    ///
    /// Holding rather than rejecting is deliberate: the Airbender proof already
    /// cost a GPU run, and the alternative — refusing the submission so the
    /// range is re-offered — pays for that run again. Nothing is lost by
    /// waiting, because a restart re-proves the range anyway.
    async fn compose_or_park(&self, command: ProofCommand) -> anyhow::Result<Composed> {
        let Some(aggregation) = self.zisk_aggregation_job_manager.as_ref() else {
            return Ok(Composed::Ready(command));
        };
        // Whatever shape it settles in, a range that leaves this stage means
        // the batches below it went downstream: both lanes need to let go of
        // what can no longer join a range. Shadow proving reaches this too —
        // it composes nothing, so this is its only settlement signal.
        let settle = |command: ProofCommand| async {
            if let (Some(last), Some(zisk_job_manager)) =
                (command.batches().last(), self.zisk_job_manager.as_ref())
            {
                zisk_job_manager
                    .on_batches_settled(last.batch_number())
                    .await;
            }
            command
        };
        // A fake Airbender proof can never be composed into a MultiProof, so
        // the range's ZiSK state — the jobs opened at seal, any parked proof —
        // would sit there forever in fake-prover environments.
        //
        // Where the multi-proof is required, settling one would put a
        // single-proof payload on a chain that rejects it and discard the ZiSK
        // half on the way. Startup validation already refuses the fake pools
        // there, so reaching this is a wiring slip, not a mode — fail loudly
        // instead of degrading silently.
        if matches!(command.proof(), SnarkProof::Fake) && self.require_multi_proof {
            anyhow::bail!(
                "a fake Airbender proof reached the range stage while the multi-proof is \
                 required: it cannot be composed, and settling it would commit a batch \
                 range with one proof"
            );
        }
        if matches!(command.proof(), SnarkProof::Fake)
            && let Some(zisk_job_manager) = self.zisk_job_manager.as_ref()
        {
            let batches = command.batches();
            if let (Some(first), Some(last)) = (batches.first(), batches.last()) {
                zisk_job_manager
                    .discard_batches(first.batch_number(), last.batch_number())
                    .await;
            }
            return Ok(Composed::Ready(command));
        }
        // Shadow proving settles Airbender-only: the aggregated proof's
        // submit-time validation is the signal, and a type-5 payload would be
        // rejected by a chain without the MultiProofVerifier deployed.
        if !self.require_multi_proof {
            return Ok(Composed::Ready(settle(command).await));
        }

        let (batches, airbender) = command.into_parts();
        let batch_from = batches
            .first()
            .expect("a proof command carries at least one batch")
            .batch_number();
        let last = batches
            .last()
            .expect("a proof command carries at least one batch");
        let batch_to = last.batch_number();
        let range = BatchRange::of(batch_from, batch_to);
        let proving_version = last.batch.proving_version()?;

        // Check the Airbender proof's shape BEFORE taking the ZiSK half: taking
        // it retires the range and advances the aggregation floor, so a proof
        // that turns out not to compose would destroy a verified artifact on
        // its way to settling without it.
        let SnarkProof::Real(RealSnarkProof::V2 { .. }) = &airbender else {
            // Only the versioned real proof composes. The multi-proof is
            // required here, so a legacy V1 shape cannot settle as itself: the
            // verifier would reject it, and taking the ZiSK half for it would
            // retire a verified range for nothing.
            anyhow::bail!(
                "a non-composable Airbender proof shape reached the range stage while the \
                 multi-proof is required"
            );
        };

        let Some(aggregated) = aggregation.take_completed(range).await else {
            // Demand the range rather than waiting for it blindly. Registration
            // is idempotent, and it is what re-forms a range that was retired
            // or abandoned — an overlapping re-pick, an exhausted attempt
            // count, a proof evicted under capacity — so the ZiSK half can
            // still be produced from the buffered inputs. Without this the hold
            // below has no release path at all.
            aggregation.note_snark_range(range).await;
            tracing::info!(
                batch_from,
                batch_to,
                "Airbender range proof is waiting for the aggregated ZiSK proof of the same range"
            );
            return Ok(Composed::Waiting(ProofCommand::new(batches, airbender)));
        };

        let SnarkProof::Real(RealSnarkProof::V2 {
            proof: payload,
            proving_execution_version: _,
        }) = airbender
        else {
            unreachable!("the proof shape was checked above");
        };
        tracing::info!(
            batch_from,
            batch_to,
            airbender_proof_bytes = payload.len(),
            zisk_proof_bytes = aggregated.proof.len(),
            "composing the Airbender + aggregated ZiSK multi-proof"
        );
        // The ZiSK public values are not carried in the L1 payload: the
        // on-chain MultiProofVerifier reconstructs and binds the aggregated
        // proof's range digest.
        // The shapes the on-chain verifiers require are checked as the two
        // proofs are put together, so a mis-shaped pair fails here rather than
        // at the L1 encoder several stages later.
        let multi_proof =
            zisk_prover_lane::compose_multiproof(payload, aggregated.proof, proving_version)?;
        Ok(Composed::Ready(
            settle(ProofCommand::new(
                batches,
                SnarkProof::MultiProof(multi_proof),
            ))
            .await,
        ))
    }
}

#[async_trait]
impl PipelineComponent for RangeProvingPipelineStep {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = L1SenderCommand<ProofCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::SnarkJobManager;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Split the state the two concurrent halves touch: the inbound half
        // only adds jobs, the outbound half owns both receivers.
        let Self {
            last_proved_batch_number,
            snark_job_manager,
            mut proof_commands_receiver,
            zisk_job_manager,
            zisk_aggregation_job_manager,
            mut zisk_range_ready,
            require_multi_proof,
        } = self;
        let composer = RangeComposer {
            zisk_job_manager,
            zisk_aggregation_job_manager,
            require_multi_proof,
        };
        // Forward batches: pipeline input → SnarkJobManager → pipeline output
        // Two concurrent tasks handle the bidirectional flow
        tokio::select! {
            result = async {
                while let Some(batch) = input.recv_and_record_picked(&state_reporter).await {
                    if batch.batch_number() > last_proved_batch_number {
                        snark_job_manager.add_job(batch).await;
                    } else {
                        let passthrough = L1SenderCommand::Passthrough(Box::new(batch));
                        output.send_and_record(passthrough, &state_reporter)?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("inbound channel closed");
                return Ok(());
            },
            result = async {
                // The Airbender range proof waiting for its ZiSK half. At most
                // one: the stage stops reading completions while it holds one,
                // which is the backpressure on the Airbender lane.
                let mut parked: Option<ProofCommand> = None;
                loop {
                    tokio::select! {
                        proof_command = proof_commands_receiver.recv(), if parked.is_none() => {
                            let Some(proof_command) = proof_command else { break };
                            match composer.compose_or_park(proof_command).await? {
                                Composed::Ready(command) => output.send_and_record(
                                    L1SenderCommand::SendToL1(command),
                                    &state_reporter,
                                )?,
                                Composed::Waiting(command) => parked = Some(command),
                            }
                        }
                        ready = async {
                            match zisk_range_ready.as_mut() {
                                Some(receiver) => receiver.recv().await,
                                // No ZiSK lane: nothing ever announces, so this
                                // branch must never resolve.
                                None => std::future::pending().await,
                            }
                        }, if parked.is_some() => {
                            if ready.is_none() {
                                break;
                            }
                            let command = parked.take().expect("guarded by `parked.is_some()`");
                            match composer.compose_or_park(command).await? {
                                Composed::Ready(command) => output.send_and_record(
                                    L1SenderCommand::SendToL1(command),
                                    &state_reporter,
                                )?,
                                Composed::Waiting(command) => parked = Some(command),
                            }
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("outbound channel closed");
                return Ok(());
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prover_api::test_util::create_test_batch_envelope_with_data;
    use std::collections::HashMap;
    use zisk_prover_lane::{
        CompletedAggregatedProof, ZiskAggregationJobManager, ZiskAggregationMode,
    };
    use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;
    use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
    use zksync_os_types::ProtocolSemanticVersion;

    fn envelope(batch_number: u64) -> SignedBatchEnvelope<FriProof> {
        create_test_batch_envelope_with_data(
            batch_number,
            ProtocolSemanticVersion::new(0, 31, 0),
            FriProof::Fake,
        )
    }

    /// An Airbender range proof for batches 1..=2, in the shape the SNARK lane
    /// submits.
    fn airbender_range_proof() -> ProofCommand {
        ProofCommand::new(
            vec![envelope(1), envelope(2)],
            SnarkProof::Real(RealSnarkProof::V2 {
                proof: vec![0xAA; 32],
                proving_execution_version: 7,
            }),
        )
    }

    fn aggregation(mode: ZiskAggregationMode) -> Arc<ZiskAggregationJobManager> {
        Arc::new(ZiskAggregationJobManager::new(
            zisk_prover_lane::ZiskAggregationLaneConfig {
                range_size: 2,
                assignment_timeout: Duration::from_secs(60),
                verification_timeout: Duration::from_secs(60),
                expected_program_vk: None,
                expected_inner_vks: HashMap::new(),
                proof_verification_enabled: false,
                mode,
            },
        ))
    }

    fn composer(
        aggregation: Option<Arc<ZiskAggregationJobManager>>,
        require_multi_proof: bool,
    ) -> RangeComposer {
        RangeComposer {
            zisk_job_manager: None,
            zisk_aggregation_job_manager: aggregation,
            require_multi_proof,
        }
    }

    /// Both halves present: the stage pairs them into the type-5 payload.
    #[tokio::test]
    async fn pairs_the_two_range_proofs() {
        let aggregation = aggregation(ZiskAggregationMode::Required {
            range_ready: mpsc::channel(4).0,
        });
        aggregation
            .park_completed_for_test(
                BatchRange::of(1, 2),
                CompletedAggregatedProof {
                    proof: vec![0xBB; ZISK_SNARK_PROOF_BYTES],
                    public_values: vec![],
                },
            )
            .await;

        let composed = composer(Some(aggregation), true)
            .compose_or_park(airbender_range_proof())
            .await
            .expect("composition must not fail");

        let Composed::Ready(command) = composed else {
            panic!("both halves were available, so the range must be ready");
        };
        let (_, proof) = command.into_parts();
        let SnarkProof::MultiProof(multi) = proof else {
            panic!("a paired range must settle as a multi-proof");
        };
        assert_eq!(multi.airbender_proof(), &[0xAA; 32]);
        assert_eq!(multi.zisk_proof().len(), ZISK_SNARK_PROOF_BYTES);
    }

    /// The ZiSK half has not been proved yet. The Airbender proof is held, not
    /// rejected: rejecting it would throw away a GPU run and buy nothing.
    #[tokio::test]
    async fn parks_until_the_zisk_half_arrives() {
        let composed = composer(
            Some(aggregation(ZiskAggregationMode::Required {
                range_ready: mpsc::channel(4).0,
            })),
            true,
        )
        .compose_or_park(airbender_range_proof())
        .await
        .expect("parking must not fail");

        let Composed::Waiting(command) = composed else {
            panic!("without the ZiSK half the range must wait");
        };
        assert!(
            matches!(command.proof(), SnarkProof::Real(_)),
            "the held proof must be the untouched Airbender one"
        );
    }

    /// Ready-channel entries are coalesced wake tokens, not authoritative
    /// range identities. If the channel is full when the matching ZiSK proof
    /// is stored, any queued token wakes the single parked Airbender range and
    /// its state re-check observes the completed proof.
    #[tokio::test]
    async fn a_full_ready_channel_still_wakes_the_parked_range() {
        let (ready_tx, mut ready_rx) = mpsc::channel(1);
        ready_tx
            .try_send(BatchRange::of(99, 100))
            .expect("the wake channel starts empty");
        let aggregation = aggregation(ZiskAggregationMode::Required {
            range_ready: ready_tx.clone(),
        });
        let composer = composer(Some(aggregation.clone()), true);
        let Composed::Waiting(parked) = composer
            .compose_or_park(airbender_range_proof())
            .await
            .expect("the Airbender proof parks")
        else {
            panic!("the ZiSK half is not present yet");
        };

        let range = BatchRange::of(1, 2);
        aggregation
            .park_completed_for_test(
                range,
                CompletedAggregatedProof {
                    proof: vec![0xBB; ZISK_SNARK_PROOF_BYTES],
                    public_values: vec![],
                },
            )
            .await;
        assert!(matches!(
            ready_tx.try_send(range),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));

        assert_eq!(ready_rx.recv().await, Some(BatchRange::of(99, 100)));
        let Composed::Ready(_) = composer
            .compose_or_park(parked)
            .await
            .expect("the queued wake triggers a state re-check")
        else {
            panic!("the stored ZiSK proof must compose despite its dropped wake token");
        };
    }

    /// Shadow proving settles Airbender-only: a type-5 payload would be
    /// rejected by a chain with no MultiProofVerifier deployed.
    #[tokio::test]
    async fn shadow_proving_passes_airbender_through() {
        let composed = composer(Some(aggregation(ZiskAggregationMode::Shadow)), false)
            .compose_or_park(airbender_range_proof())
            .await
            .expect("pass-through must not fail");

        let Composed::Ready(command) = composed else {
            panic!("shadow proving never waits for the second lane");
        };
        assert!(matches!(command.proof(), SnarkProof::Real(_)));
    }

    /// With no second proof system configured the stage is upstream's.
    #[tokio::test]
    async fn without_the_second_lane_nothing_changes() {
        let composed = composer(None, false)
            .compose_or_park(airbender_range_proof())
            .await
            .expect("pass-through must not fail");
        assert!(matches!(composed, Composed::Ready(_)));
    }
}
