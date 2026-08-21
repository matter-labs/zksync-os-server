//! Assembly of the second proof system.
//!
//! Three config switches decide whether ZiSK runs and whether settlement waits
//! for it. Resolving them once, here, keeps the answer from being re-derived
//! differently at startup, in either proving stage, or on the seal path.
//!
//! Build order matters: the channels first, then the aggregation lane that
//! announces on one of them, then the per-batch lane that feeds it. That order
//! is what lets every collaborator arrive through a constructor.

use std::sync::Arc;
use tokio::sync::mpsc;
use zisk_prover_lane::{
    BatchRange, ZiskAggregationJobManager, ZiskAggregationLaneConfig, ZiskAggregationMode,
    ZiskJobManager, ZiskLaneConfig, ZiskLaneMode, ZiskLaneWiring, ZiskVkSet,
};
use zksync_os_types::ProtocolSemanticVersion;

use crate::batcher::zisk_batch::{SecondProofSystemConfig, ShadowConfig, ZiskChainConfig};
use crate::config::Config;
use crate::prover_api::batch_proving_pipeline_step::{
    CommitGateConfig, MAX_READY_BATCH_SIGNALS, SecondProofBatchStage,
};
use crate::prover_api::range_proving_pipeline_step::{
    MAX_READY_RANGE_SIGNALS, SecondProofRangeStage,
};

/// The second proof system as the node runs it.
///
/// `Shadow` proves every batch and range and verifies what comes back, but
/// settlement never waits: a lost proof costs coverage. `Required` puts the
/// proof on L1, so a batch may not be committed before this lane has proved it
/// — which is why only that variant carries a commit gate.
pub enum SecondProofRuntime {
    Disabled,
    Shadow(Lanes),
    Required {
        lanes: Lanes,
        /// The channels the two stages wait on. Only `Required` has them,
        /// because only `Required` waits.
        ready: ReadyChannels,
        gate: CommitGateConfig,
    },
}

/// The two managers, plus what the batcher and the halt task need.
pub struct Lanes {
    pub per_batch: Arc<ZiskJobManager>,
    pub aggregation: Arc<ZiskAggregationJobManager>,
    /// What the batcher needs at seal to build the witness.
    pub seal: SecondProofSystemConfig,
    /// Fires once if a verified, locally corroborated commitment mismatch is
    /// found. `None` unless halt-on-mismatch is configured.
    pub halt: Option<tokio::sync::oneshot::Receiver<String>>,
}

/// What each stage waits on under a required multi-proof.
pub struct ReadyChannels {
    /// Batches whose second proof arrived — the only notice the commit gate
    /// gets.
    pub batch: mpsc::Receiver<u64>,
    /// Ranges whose aggregated proof parked, for the stage that composes.
    pub range: mpsc::Receiver<BatchRange>,
}

/// The managers the prover API serves from. Cloned rather than moved, since
/// the pipeline stages take the same `Arc`s.
pub struct SecondProofHandles {
    pub per_batch: Arc<ZiskJobManager>,
    pub aggregation: Arc<ZiskAggregationJobManager>,
}

impl SecondProofRuntime {
    /// Build from config. `merkle_tree` and `last_proved_batch` are the seal
    /// path's inputs; everything else is read from config.
    pub fn new(
        config: &Config,
        chain_id: u64,
        merkle_tree: zksync_os_merkle_tree::MerkleTree<zksync_os_merkle_tree::RocksDBWrapper>,
        last_proved_batch: u64,
    ) -> Self {
        if !config.prover_input_generator_config.second_proof_system {
            return Self::Disabled;
        }

        let required = config.prover_input_generator_config.multi_proof_verifier;
        let chain_config = ZiskChainConfig {
            fri_proof_verification_enabled: config.genesis_config.fri_proof_verification_enabled,
            max_tx_gas_limit: config.genesis_config.max_tx_gas_limit,
        };
        let expected_vks: std::collections::HashMap<ProtocolSemanticVersion, ZiskVkSet> = config
            .prover_api_config
            .zisk_vks
            .iter()
            .map(|entry| {
                (
                    entry.protocol_version.clone(),
                    ZiskVkSet {
                        program_vk: entry.program_vk,
                        vadcop_vk: entry.vadcop_vk,
                    },
                )
            })
            .collect();

        // Dependency order: channels, then the lane that announces on them,
        // then the lane that feeds it. Each mode carries its own channels, so
        // a lane can only be built with what its mode needs.
        let (halt_tx, halt) = if config
            .prover_input_generator_config
            .halt_on_zisk_commitment_mismatch
        {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let agg_config = &config.prover_api_config.zisk_aggregation;
        // A range covers exactly one Airbender SNARK job, so the range width is
        // `max_fris_per_snark` — one source of truth, not a separate knob.
        let range_size = config.prover_api_config.max_fris_per_snark;
        let verify = config.prover_api_config.zisk_proof_verification_enabled;

        let (aggregation_mode, lane_mode, ready) = if required {
            let (batch_tx, batch) = mpsc::channel(MAX_READY_BATCH_SIGNALS);
            let (range_tx, range) = mpsc::channel(MAX_READY_RANGE_SIGNALS);
            (
                ZiskAggregationMode::Required {
                    range_ready: range_tx,
                },
                ZiskLaneMode::Required {
                    batch_ready: batch_tx,
                },
                Some(ReadyChannels { batch, range }),
            )
        } else {
            (ZiskAggregationMode::Shadow, ZiskLaneMode::Shadow, None)
        };

        let aggregation = Arc::new(ZiskAggregationJobManager::new(ZiskAggregationLaneConfig {
            range_size,
            assignment_timeout: agg_config.job_timeout,
            verification_timeout: agg_config.verification_timeout,
            expected_program_vk: agg_config.program_vk,
            expected_inner_vks: expected_vks.clone(),
            proof_verification_enabled: verify,
            mode: aggregation_mode,
        }));
        let per_batch = Arc::new(ZiskJobManager::new(
            ZiskLaneConfig {
                assignment_timeout: config.prover_api_config.snark_job_timeout,
                expected_vks,
                chain_id,
                proof_verification_enabled: verify,
            },
            ZiskLaneWiring {
                aggregation_sink: aggregation.clone(),
                mode: lane_mode,
                halt_on_mismatch: halt_tx,
            },
        ));

        tracing::info!(
            required,
            range_size,
            aggregator_program_vk = ?agg_config.program_vk,
            pinned_versions = config.prover_api_config.zisk_vks.len(),
            "second proof system enabled"
        );

        let lanes = Lanes {
            per_batch,
            aggregation,
            seal: SecondProofSystemConfig {
                chain_config,
                merkle_tree,
                shadow: config
                    .prover_input_generator_config
                    .zisk_shadow_execution
                    .then_some(ShadowConfig {
                        halt_on_mismatch: config
                            .prover_input_generator_config
                            .halt_on_zisk_commitment_mismatch,
                    }),
                required,
                last_proved_batch,
            },
            halt,
        };

        match ready {
            Some(ready) => Self::Required {
                lanes,
                ready,
                gate: CommitGateConfig {
                    admission_window: config.prover_api_config.commit_gate_admission_window,
                },
            },
            None => Self::Shadow(lanes),
        }
    }

    /// The managers the prover API serves from.
    pub fn handles(&self) -> Option<SecondProofHandles> {
        self.lanes().map(|lanes| SecondProofHandles {
            per_batch: lanes.per_batch.clone(),
            aggregation: lanes.aggregation.clone(),
        })
    }

    /// What the batcher needs at seal, when there is a second system at all.
    pub fn seal_config(&self) -> Option<SecondProofSystemConfig> {
        self.lanes().map(|lanes| lanes.seal.clone())
    }

    /// The halt receiver, taken once by the task that watches it.
    pub fn take_halt(&mut self) -> Option<tokio::sync::oneshot::Receiver<String>> {
        match self {
            Self::Disabled => None,
            Self::Shadow(lanes) => lanes.halt.take(),
            Self::Required { lanes, .. } => lanes.halt.take(),
        }
    }

    fn lanes(&self) -> Option<&Lanes> {
        match self {
            Self::Disabled => None,
            Self::Shadow(lanes) | Self::Required { lanes, .. } => Some(lanes),
        }
    }

    /// Split into what each proving stage needs. By value: the channels can
    /// only be taken once, and each stage gets a value that already answers
    /// what its mode requires.
    pub fn into_stages(self) -> (SecondProofBatchStage, SecondProofRangeStage) {
        match self {
            Self::Disabled => (
                SecondProofBatchStage::Disabled,
                SecondProofRangeStage::Disabled,
            ),
            Self::Shadow(lanes) => (
                SecondProofBatchStage::Shadow {
                    manager: lanes.per_batch.clone(),
                },
                SecondProofRangeStage::Shadow {
                    per_batch: lanes.per_batch,
                    aggregation: lanes.aggregation,
                },
            ),
            Self::Required { lanes, ready, gate } => (
                SecondProofBatchStage::Required {
                    manager: lanes.per_batch.clone(),
                    ready: ready.batch,
                    gate,
                },
                SecondProofRangeStage::Required {
                    per_batch: lanes.per_batch,
                    aggregation: lanes.aggregation,
                    ready: ready.range,
                },
            ),
        }
    }
}
