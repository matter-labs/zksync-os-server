use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

pub use zksync_os_pipeline::ComponentId;

/// Internal evaluation condition used by the monitor's evaluate() logic.
/// Constructed from the public config types by condition_for().
/// Not part of the public API — callers configure via BlockPipelineCondition /
/// BatchPipelineCondition.
pub struct BackpressureCondition {
    pub max_block_lag: Option<u64>,
    pub max_time_lag: Option<Duration>,
}

/// Backpressure thresholds for block-level pipeline components:
/// BlockExecutor, BlockCanonizer, BlockApplier, TreeManager, ProverInputGenerator.
///
/// Both fields are applicable: blocks flow one at a time, so both block count and
/// wall-clock time are meaningful lag measures.
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct BlockPipelineCondition {
    /// Trigger backpressure when a block-pipeline component is more than N blocks behind
    /// its upstream neighbour. Every monitored component must have an adjacency pair registered;
    /// the monitor asserts this at startup.
    pub max_block_lag: Option<u64>,
    /// Trigger backpressure when the block-timestamp lag for any block-pipeline component
    /// exceeds this duration. Only evaluated when both head and component timestamps are
    /// available (Some).
    pub max_time_lag: Option<Duration>,
}

/// Backpressure thresholds for batch-level pipeline components:
/// Batcher, BatchVerification, FriJobManager, GaplessCommitter, UpgradeGatekeeper,
/// L1SenderCommit, SnarkJobManager, GaplessL1ProofSender, L1SenderProve,
/// PriorityTree, L1SenderExecute.
///
/// `max_block_lag` is available but use it carefully: block lag for batch components
/// oscillates 0→batch_size→0 during normal accumulation. Set the threshold well above
/// your expected batch size to avoid false positives. Components that submit work
/// concurrently (FriJobManager, SnarkJobManager, L1SenderCommit, L1SenderProve,
/// L1SenderExecute) use high-watermark reporting, so out-of-order completions will
/// not walk the watermark backward and create false positives.
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct BatchPipelineCondition {
    /// Trigger backpressure when a batch-pipeline component is more than N blocks
    /// behind the pipeline head. Use a threshold well above your typical batch size
    /// to avoid false positives during normal batch accumulation.
    pub max_block_lag: Option<u64>,
    /// Trigger backpressure when the block-timestamp lag for any batch-pipeline component
    /// exceeds this duration. Only evaluated when the component has reported a timestamp
    /// via record_processed(block_number, Some(timestamp)).
    pub max_time_lag: Option<Duration>,
}

/// Backpressure thresholds for a single component, overriding its group default.
///
/// When set, this condition **completely replaces** the group condition for that component:
/// unset fields (`None`) mean "no threshold" for that component, not "inherit from group".
///
/// To disable backpressure entirely for a component, set `enabled = false` — this is the
/// only way to express an all-None override, since an absent section is indistinguishable
/// from no override at all.
///
/// Note: when setting `max_block_lag` on batch-pipeline components via an override,
/// use a threshold well above your typical batch size — the block lag oscillates
/// 0→batch_size→0 during normal operation. See `BatchPipelineCondition` for details.
#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
pub struct ComponentConditionOverride {
    /// When false, backpressure is completely disabled for this component regardless
    /// of the other fields. Default: true.
    #[config(default_t = true)]
    pub enabled: bool,
    /// Override the block-lag threshold for this component.
    pub max_block_lag: Option<u64>,
    /// Override the time-lag threshold for this component.
    pub max_time_lag: Option<Duration>,
}

/// Per-component backpressure condition overrides. Any component listed here has its
/// group condition (block_pipeline or batch_pipeline) fully replaced by this override.
/// Omitted components continue to use their group default.
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct ComponentOverrides {
    #[config(nest)]
    pub block_executor: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub block_canonizer: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub block_applier: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub tree_manager: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub prover_input_generator: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub batch_verification_responder: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub batcher: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub batch_verification: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub fri_job_manager: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub gapless_committer: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub upgrade_gatekeeper: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub l1_sender_commit: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub snark_job_manager: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub gapless_l1_proof_sender: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub l1_sender_prove: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub priority_tree: Option<ComponentConditionOverride>,
    #[config(nest)]
    pub l1_sender_execute: Option<ComponentConditionOverride>,
}

impl ComponentOverrides {
    fn get(&self, id: ComponentId) -> Option<&ComponentConditionOverride> {
        match id {
            ComponentId::BlockExecutor => self.block_executor.as_ref(),
            ComponentId::BlockCanonizer => self.block_canonizer.as_ref(),
            ComponentId::BlockApplier => self.block_applier.as_ref(),
            ComponentId::TreeManager => self.tree_manager.as_ref(),
            ComponentId::ProverInputGenerator => self.prover_input_generator.as_ref(),
            ComponentId::BatchVerificationResponder => self.batch_verification_responder.as_ref(),
            ComponentId::Batcher => self.batcher.as_ref(),
            ComponentId::BatchVerification => self.batch_verification.as_ref(),
            ComponentId::FriJobManager => self.fri_job_manager.as_ref(),
            ComponentId::GaplessCommitter => self.gapless_committer.as_ref(),
            ComponentId::UpgradeGatekeeper => self.upgrade_gatekeeper.as_ref(),
            ComponentId::L1SenderCommit => self.l1_sender_commit.as_ref(),
            ComponentId::SnarkJobManager => self.snark_job_manager.as_ref(),
            ComponentId::GaplessL1ProofSender => self.gapless_l1_proof_sender.as_ref(),
            ComponentId::L1SenderProve => self.l1_sender_prove.as_ref(),
            ComponentId::PriorityTree => self.priority_tree.as_ref(),
            ComponentId::L1SenderExecute => self.l1_sender_execute.as_ref(),
            // Unmonitored components have no per-component override config.
            ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BatchSink
            | ComponentId::NoOpSink
            | ComponentId::RevmConsistencyChecker => None,
        }
    }
}

/// Pipeline health backpressure configuration.
///
/// Configure one or both groups; a group left at default (all None) applies no backpressure
/// for its components. Use `component_overrides` to tune individual components.
///
/// Example — halt new transactions if block execution falls more than 50 blocks behind:
///   pipeline_health.block_pipeline.max_block_lag = 50
///
/// Example — halt new transactions if any batch stage is more than 30 minutes stale:
///   pipeline_health.batch_pipeline.max_time_lag = "30m"
///
/// Example — give l1_sender_commit a longer time_lag budget than other batch components:
///   pipeline_health.batch_pipeline.max_time_lag = "30m"
///   pipeline_health.component_overrides.l1_sender_commit.max_time_lag = "2h"
///
/// Example — disable backpressure for a specific component entirely:
///   pipeline_health.component_overrides.priority_tree.max_time_lag =  (leave unset)
#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
#[config(derive(Default))]
pub struct PipelineHealthConfig {
    /// Thresholds applied to block-level components.
    #[config(nest, default)]
    pub block_pipeline: BlockPipelineCondition,
    /// Thresholds applied to batch-level components.
    /// See `BatchPipelineCondition` for guidance on using `max_block_lag` with batch components.
    #[config(nest, default)]
    pub batch_pipeline: BatchPipelineCondition,
    /// Per-component overrides. A component listed here has its group condition fully
    /// replaced by the override. Omitted components use the group default.
    #[config(nest, default)]
    pub component_overrides: ComponentOverrides,
    /// How often to refresh Prometheus metrics regardless of health change events.
    /// Default: 5 seconds.
    #[config(default_t = Duration::from_secs(5))]
    pub metrics_interval: Duration,
}

impl PipelineHealthConfig {
    /// Returns the effective backpressure condition for `id`.
    ///
    /// Precedence (highest to lowest):
    /// 1. `component_overrides` — if present, fully replaces the group condition.
    /// 2. Group default — block-pipeline components use `block_pipeline`;
    ///    batch-pipeline components use `batch_pipeline`.
    pub fn condition_for(&self, id: ComponentId) -> BackpressureCondition {
        // Per-component override takes full precedence over the group default.
        if let Some(o) = self.component_overrides.get(id) {
            return if o.enabled {
                BackpressureCondition {
                    max_block_lag: o.max_block_lag,
                    max_time_lag: o.max_time_lag,
                }
            } else {
                BackpressureCondition {
                    max_block_lag: None,
                    max_time_lag: None,
                }
            };
        }

        match id {
            ComponentId::BlockExecutor
            | ComponentId::BlockApplier
            | ComponentId::TreeManager
            | ComponentId::BlockCanonizer
            | ComponentId::ProverInputGenerator
            | ComponentId::BatchVerificationResponder => BackpressureCondition {
                max_block_lag: self.block_pipeline.max_block_lag,
                max_time_lag: self.block_pipeline.max_time_lag,
            },
            // All batch-level components use time lag as the primary signal.
            // max_block_lag is also supported but must be set well above batch_size
            // to avoid oscillation false-positives (see BatchPipelineCondition docs).
            ComponentId::Batcher
            | ComponentId::BatchVerification
            | ComponentId::FriJobManager
            | ComponentId::GaplessCommitter
            | ComponentId::UpgradeGatekeeper
            | ComponentId::L1SenderCommit
            | ComponentId::SnarkJobManager
            | ComponentId::GaplessL1ProofSender
            | ComponentId::L1SenderProve
            | ComponentId::PriorityTree
            | ComponentId::L1SenderExecute => BackpressureCondition {
                max_block_lag: self.batch_pipeline.max_block_lag,
                max_time_lag: self.batch_pipeline.max_time_lag,
            },
            // Unmonitored components are never subject to backpressure.
            ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BatchSink
            | ComponentId::NoOpSink
            | ComponentId::RevmConsistencyChecker => BackpressureCondition {
                max_block_lag: None,
                max_time_lag: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_conditions_are_all_none() {
        let config = PipelineHealthConfig::default();
        let block = config.condition_for(ComponentId::BlockExecutor);
        assert!(block.max_block_lag.is_none());
        assert!(block.max_time_lag.is_none());
        let batch = config.condition_for(ComponentId::Batcher);
        assert!(batch.max_block_lag.is_none());
        assert!(batch.max_time_lag.is_none());
    }

    #[test]
    fn block_pipeline_condition_applies_to_all_block_components() {
        let config = PipelineHealthConfig {
            block_pipeline: BlockPipelineCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            ..Default::default()
        };
        for id in [
            ComponentId::BlockExecutor,
            ComponentId::BlockApplier,
            ComponentId::TreeManager,
            ComponentId::BlockCanonizer,
            ComponentId::ProverInputGenerator,
        ] {
            let cond = config.condition_for(id);
            assert_eq!(cond.max_block_lag, Some(50), "failed for {id:?}");
            assert!(cond.max_time_lag.is_none(), "failed for {id:?}");
        }
    }

    #[test]
    fn batch_pipeline_condition_applies_to_all_batch_components() {
        let config = PipelineHealthConfig {
            batch_pipeline: BatchPipelineCondition {
                max_block_lag: None,
                max_time_lag: Some(Duration::from_secs(300)),
            },
            ..Default::default()
        };
        for id in [
            ComponentId::Batcher,
            ComponentId::BatchVerification,
            ComponentId::FriJobManager,
            ComponentId::GaplessCommitter,
            ComponentId::UpgradeGatekeeper,
            ComponentId::L1SenderCommit,
            ComponentId::SnarkJobManager,
            ComponentId::GaplessL1ProofSender,
            ComponentId::L1SenderProve,
            ComponentId::PriorityTree,
            ComponentId::L1SenderExecute,
        ] {
            let cond = config.condition_for(id);
            assert!(
                cond.max_block_lag.is_none(),
                "batch component {id:?} must not get max_block_lag when BatchPipelineCondition.max_block_lag is None"
            );
            assert_eq!(
                cond.max_time_lag,
                Some(Duration::from_secs(300)),
                "failed for {id:?}"
            );
        }
    }

    #[test]
    fn batch_components_do_not_inherit_block_lag_from_block_pipeline_config() {
        let config = PipelineHealthConfig {
            block_pipeline: BlockPipelineCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            ..Default::default()
        };
        let cond = config.condition_for(ComponentId::Batcher);
        assert!(
            cond.max_block_lag.is_none(),
            "Batcher must not get max_block_lag from block_pipeline config"
        );
    }

    #[test]
    fn condition_for_all_variants_does_not_panic() {
        let config = PipelineHealthConfig::default();
        use ComponentId::*;
        for id in [
            BlockExecutor,
            BlockApplier,
            TreeManager,
            BlockCanonizer,
            ProverInputGenerator,
            Batcher,
            BatchVerification,
            FriJobManager,
            GaplessCommitter,
            UpgradeGatekeeper,
            L1SenderCommit,
            SnarkJobManager,
            GaplessL1ProofSender,
            L1SenderProve,
            PriorityTree,
            L1SenderExecute,
        ] {
            let _ = config.condition_for(id);
        }
    }

    #[test]
    fn component_override_fully_replaces_group_condition() {
        // Block component: group says max_block_lag=50, override gives it only time lag
        let config = PipelineHealthConfig {
            block_pipeline: BlockPipelineCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            component_overrides: ComponentOverrides {
                block_executor: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_lag: None,
                    max_time_lag: Some(Duration::from_secs(30)),
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::BlockExecutor);
        // Override replaces: no block lag, has time lag
        assert!(cond.max_block_lag.is_none());
        assert_eq!(cond.max_time_lag, Some(Duration::from_secs(30)));

        // Sibling not overridden still gets group defaults
        let cond = config.condition_for(ComponentId::BlockCanonizer);
        assert_eq!(cond.max_block_lag, Some(50));
        assert!(cond.max_time_lag.is_none());
    }

    #[test]
    fn component_override_enabled_false_disables_backpressure() {
        // enabled=false fully silences the component regardless of group config
        let config = PipelineHealthConfig {
            batch_pipeline: BatchPipelineCondition {
                max_block_lag: None,
                max_time_lag: Some(Duration::from_secs(300)),
            },
            component_overrides: ComponentOverrides {
                batcher: Some(ComponentConditionOverride {
                    enabled: false,
                    max_block_lag: None,
                    max_time_lag: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::Batcher);
        assert!(cond.max_block_lag.is_none());
        assert!(cond.max_time_lag.is_none());

        // Other batch components still have the group time lag
        let cond = config.condition_for(ComponentId::FriJobManager);
        assert_eq!(cond.max_time_lag, Some(Duration::from_secs(300)));
    }

    #[test]
    fn component_override_enabled_true_with_all_none_thresholds_produces_no_backpressure() {
        // enabled=true (default) but all thresholds None → no backpressure for that component
        let config = PipelineHealthConfig {
            block_pipeline: BlockPipelineCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            component_overrides: ComponentOverrides {
                tree_manager: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_lag: None,
                    max_time_lag: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::TreeManager);
        assert!(cond.max_block_lag.is_none());
        assert!(cond.max_time_lag.is_none());
    }

    #[test]
    fn component_override_can_give_batch_component_stricter_threshold() {
        let config = PipelineHealthConfig {
            batch_pipeline: BatchPipelineCondition {
                max_block_lag: None,
                max_time_lag: Some(Duration::from_secs(300)),
            },
            component_overrides: ComponentOverrides {
                l1_sender_commit: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_lag: None,
                    max_time_lag: Some(Duration::from_secs(60)),
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::L1SenderCommit);
        assert_eq!(cond.max_time_lag, Some(Duration::from_secs(60)));

        // Others use group default
        let cond = config.condition_for(ComponentId::GaplessCommitter);
        assert_eq!(cond.max_time_lag, Some(Duration::from_secs(300)));
    }

    #[test]
    fn batch_pipeline_block_lag_applies_to_all_batch_components() {
        let config = PipelineHealthConfig {
            batch_pipeline: BatchPipelineCondition {
                max_block_lag: Some(200),
                max_time_lag: None,
            },
            ..Default::default()
        };
        for id in [
            ComponentId::Batcher,
            ComponentId::BatchVerification,
            ComponentId::FriJobManager,
            ComponentId::GaplessCommitter,
            ComponentId::UpgradeGatekeeper,
            ComponentId::L1SenderCommit,
            ComponentId::SnarkJobManager,
            ComponentId::GaplessL1ProofSender,
            ComponentId::L1SenderProve,
            ComponentId::PriorityTree,
            ComponentId::L1SenderExecute,
        ] {
            let cond = config.condition_for(id);
            assert_eq!(
                cond.max_block_lag,
                Some(200),
                "batch component {id:?} must receive max_block_lag from BatchPipelineCondition"
            );
        }
    }

    #[test]
    fn batch_pipeline_block_lag_does_not_affect_block_components() {
        let config = PipelineHealthConfig {
            batch_pipeline: BatchPipelineCondition {
                max_block_lag: Some(200),
                max_time_lag: None,
            },
            ..Default::default()
        };
        for id in [
            ComponentId::BlockExecutor,
            ComponentId::BlockApplier,
            ComponentId::TreeManager,
            ComponentId::BlockCanonizer,
            ComponentId::ProverInputGenerator,
        ] {
            let cond = config.condition_for(id);
            assert!(
                cond.max_block_lag.is_none(),
                "block component {id:?} must not receive max_block_lag from batch_pipeline config"
            );
        }
    }

    #[test]
    fn batch_pipeline_default_block_lag_is_none() {
        let config = PipelineHealthConfig::default();
        let cond = config.condition_for(ComponentId::Batcher);
        assert!(cond.max_block_lag.is_none());
    }
}
