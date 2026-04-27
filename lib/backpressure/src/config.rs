use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

pub use zksync_os_pipeline::ComponentId;

/// Effective backpressure thresholds for a single component, as resolved by
/// [`BackpressureConfig::condition_for`]. Configuration is supplied via
/// [`PipelineCondition`] (per pipeline group) and per-component
/// [`ComponentConditionOverride`]s; this struct is the flattened read-side view.
pub struct BackpressureCondition {
    pub max_block_diff_to_upstream: Option<u64>,
    pub max_time_diff_to_upstream: Option<Duration>,
    pub max_batch_diff_to_upstream: Option<u64>,
}

/// Backpressure thresholds for a pipeline group. `BackpressureConfig` holds two —
/// `block_pipeline` and `batch_pipeline` — and `condition_for` dispatches each component
/// to one of them.
///
/// Block-pipeline components (classified via `condition_for`): BlockExecutor, BlockCanonizer,
/// BlockApplier, TreeManager, ProverInputGenerator, BatchVerificationResponder, Batcher.
/// Batcher is grouped here because its upstream (ProverInputGenerator) is block-level.
///
/// Batch-pipeline components: BatchVerification, FriJobManager, GaplessCommitter,
/// UpgradeGatekeeper, L1SenderCommit, SnarkJobManager, GaplessL1ProofSender, L1SenderProve,
/// PriorityTree, L1SenderExecute.
///
/// `max_block_diff_to_upstream` reflects true adjacent channel occupancy for both groups:
/// every component records `picked` on each block it dequeues, so in steady state the diff
/// stays near 0. A sustained non-zero value indicates a real bottleneck. Batch-pipeline
/// components that submit work concurrently (FriJobManager, SnarkJobManager, L1SenderCommit,
/// L1SenderProve, L1SenderExecute) use high-watermark reporting, so out-of-order completions
/// will not walk the watermark backward and create false positives.
///
/// `max_batch_diff_to_upstream` is meaningful only for batch-pipeline components; block-pipeline
/// upstreams carry no batch_number, so `condition_for` forces it to `None` for that group.
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct PipelineCondition {
    /// Trigger backpressure when a component is more than N blocks behind its upstream
    /// neighbour. Every monitored component must have an adjacency pair registered; the
    /// monitor asserts this at startup.
    pub max_block_diff_to_upstream: Option<u64>,
    /// Trigger backpressure when the block-timestamp lag for any component exceeds this
    /// duration. Only evaluated when both head and component timestamps are available (Some).
    pub max_time_diff_to_upstream: Option<Duration>,
    /// Trigger backpressure when a component has more than N batches queued between it and
    /// its upstream neighbour. Computed as upstream.batch_processed − downstream.batch_picked.
    /// Ignored for block-pipeline components — see struct-level docs.
    pub max_batch_diff_to_upstream: Option<u64>,
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
/// Note: `max_block_diff_to_upstream` measures adjacent channel occupancy — the diff between
/// upstream and downstream picked watermarks. It stays near 0 in steady state;
/// a sustained non-zero value signals a real bottleneck.
#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
pub struct ComponentConditionOverride {
    /// When false, backpressure is completely disabled for this component regardless
    /// of the other fields. Default: true.
    #[config(default_t = true)]
    pub enabled: bool,
    /// Override the block-diff-to-upstream threshold for this component.
    pub max_block_diff_to_upstream: Option<u64>,
    /// Override the time-diff-to-upstream threshold for this component.
    pub max_time_diff_to_upstream: Option<Duration>,
    /// Override the batch-diff-to-upstream threshold for this component (batch-pipeline components only).
    pub max_batch_diff_to_upstream: Option<u64>,
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
            | ComponentId::NoopSink
            | ComponentId::RevmConsistencyChecker => None,
        }
    }
}

/// Backpressure configuration.
///
/// Configure one or both groups; a group left at default (all None) applies no backpressure
/// for its components. Use `component_overrides` to tune individual components.
///
/// Example — halt new transactions if block execution falls more than 50 blocks behind:
///   backpressure.block_pipeline.max_block_diff_to_upstream = 50
///
/// Example — halt new transactions if any batch stage is more than 30 minutes stale:
///   backpressure.batch_pipeline.max_time_diff_to_upstream = "30m"
///
/// Example — give l1_sender_commit a longer time-diff-to-upstream budget than other batch components:
///   backpressure.batch_pipeline.max_time_diff_to_upstream = "30m"
///   backpressure.component_overrides.l1_sender_commit.max_time_diff_to_upstream = "2h"
///
/// Example — disable backpressure for a specific component entirely:
///   backpressure.component_overrides.priority_tree.enabled = false
#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
#[config(derive(Default))]
pub struct BackpressureConfig {
    /// Thresholds applied to block-level components. `max_batch_diff_to_upstream` is
    /// ignored here (forced to `None` by `condition_for` because block-pipeline upstreams
    /// carry no batch coordinate).
    #[config(nest, default)]
    pub block_pipeline: PipelineCondition,
    /// Thresholds applied to batch-level components. See `PipelineCondition` docs for
    /// guidance on using `max_block_diff_to_upstream` with batch components.
    #[config(nest, default)]
    pub batch_pipeline: PipelineCondition,
    /// Per-component overrides. A component listed here has its group condition fully
    /// replaced by the override. Omitted components use the group default.
    #[config(nest, default)]
    pub component_overrides: ComponentOverrides,
}

impl BackpressureConfig {
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
                    max_block_diff_to_upstream: o.max_block_diff_to_upstream,
                    max_time_diff_to_upstream: o.max_time_diff_to_upstream,
                    max_batch_diff_to_upstream: o.max_batch_diff_to_upstream,
                }
            } else {
                BackpressureCondition {
                    max_block_diff_to_upstream: None,
                    max_time_diff_to_upstream: None,
                    max_batch_diff_to_upstream: None,
                }
            };
        }

        match id {
            // Batcher is classified with block-pipeline components because its upstream
            // (ProverInputGenerator) is block-level: block and time diffs to upstream are measured
            // between block coordinates and fall in the same magnitude as other block
            // components, not in the batch-pipeline range (which is tuned for minute-scale
            // waits like L1 inclusion or proof generation). max_batch_diff_to_upstream is None because
            // batch_diff is structurally uncomputable here — the upstream carries no
            // batch_number.
            ComponentId::BlockExecutor
            | ComponentId::BlockApplier
            | ComponentId::TreeManager
            | ComponentId::BlockCanonizer
            | ComponentId::ProverInputGenerator
            | ComponentId::BatchVerificationResponder
            | ComponentId::Batcher => BackpressureCondition {
                max_block_diff_to_upstream: self.block_pipeline.max_block_diff_to_upstream,
                max_time_diff_to_upstream: self.block_pipeline.max_time_diff_to_upstream,
                max_batch_diff_to_upstream: None,
            },
            // All batch-level components track block, time, and batch diff to upstream with
            // true adjacent channel-occupancy semantics; see PipelineCondition docs.
            ComponentId::BatchVerification
            | ComponentId::FriJobManager
            | ComponentId::GaplessCommitter
            | ComponentId::UpgradeGatekeeper
            | ComponentId::L1SenderCommit
            | ComponentId::SnarkJobManager
            | ComponentId::GaplessL1ProofSender
            | ComponentId::L1SenderProve
            | ComponentId::PriorityTree
            | ComponentId::L1SenderExecute => BackpressureCondition {
                max_block_diff_to_upstream: self.batch_pipeline.max_block_diff_to_upstream,
                max_time_diff_to_upstream: self.batch_pipeline.max_time_diff_to_upstream,
                max_batch_diff_to_upstream: self.batch_pipeline.max_batch_diff_to_upstream,
            },
            // Unmonitored components are never subject to backpressure.
            ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BatchSink
            | ComponentId::NoopSink
            | ComponentId::RevmConsistencyChecker => BackpressureCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_conditions_are_all_none() {
        let config = BackpressureConfig::default();
        let block = config.condition_for(ComponentId::BlockExecutor);
        assert!(block.max_block_diff_to_upstream.is_none());
        assert!(block.max_time_diff_to_upstream.is_none());
        let batch = config.condition_for(ComponentId::FriJobManager);
        assert!(batch.max_block_diff_to_upstream.is_none());
        assert!(batch.max_time_diff_to_upstream.is_none());
    }

    #[test]
    fn block_pipeline_condition_applies_to_all_block_components() {
        let config = BackpressureConfig {
            block_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(50),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
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
            assert_eq!(
                cond.max_block_diff_to_upstream,
                Some(50),
                "failed for {id:?}"
            );
            assert!(
                cond.max_time_diff_to_upstream.is_none(),
                "failed for {id:?}"
            );
        }
    }

    #[test]
    fn batch_pipeline_condition_applies_to_all_batch_components() {
        let config = BackpressureConfig {
            batch_pipeline: PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: Some(Duration::from_secs(300)),
                max_batch_diff_to_upstream: None,
            },
            ..Default::default()
        };
        for id in [
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
                cond.max_block_diff_to_upstream.is_none(),
                "batch component {id:?} must not get max_block_diff_to_upstream when PipelineCondition.max_block_diff_to_upstream is None"
            );
            assert_eq!(
                cond.max_time_diff_to_upstream,
                Some(Duration::from_secs(300)),
                "failed for {id:?}"
            );
        }
    }

    #[test]
    fn batch_components_do_not_inherit_block_diff_to_upstream_from_block_pipeline_config() {
        let config = BackpressureConfig {
            block_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(50),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..Default::default()
        };
        let cond = config.condition_for(ComponentId::FriJobManager);
        assert!(
            cond.max_block_diff_to_upstream.is_none(),
            "FriJobManager must not get max_block_diff_to_upstream from block_pipeline config"
        );
    }

    #[test]
    fn component_override_fully_replaces_group_condition() {
        // Block component: group says max_block_diff_to_upstream=50, override gives it only time diff
        let config = BackpressureConfig {
            block_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(50),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            component_overrides: ComponentOverrides {
                block_executor: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_diff_to_upstream: None,
                    max_time_diff_to_upstream: Some(Duration::from_secs(30)),
                    max_batch_diff_to_upstream: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::BlockExecutor);
        // Override replaces: no block diff, has time diff
        assert!(cond.max_block_diff_to_upstream.is_none());
        assert_eq!(
            cond.max_time_diff_to_upstream,
            Some(Duration::from_secs(30))
        );

        // Sibling not overridden still gets group defaults
        let cond = config.condition_for(ComponentId::BlockCanonizer);
        assert_eq!(cond.max_block_diff_to_upstream, Some(50));
        assert!(cond.max_time_diff_to_upstream.is_none());
    }

    #[test]
    fn component_override_enabled_false_disables_backpressure() {
        // enabled=false fully silences the component regardless of group config
        let config = BackpressureConfig {
            batch_pipeline: PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: Some(Duration::from_secs(300)),
                max_batch_diff_to_upstream: None,
            },
            component_overrides: ComponentOverrides {
                batcher: Some(ComponentConditionOverride {
                    enabled: false,
                    max_block_diff_to_upstream: None,
                    max_time_diff_to_upstream: None,
                    max_batch_diff_to_upstream: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::Batcher);
        assert!(cond.max_block_diff_to_upstream.is_none());
        assert!(cond.max_time_diff_to_upstream.is_none());

        // Other batch components still have the group time diff
        let cond = config.condition_for(ComponentId::FriJobManager);
        assert_eq!(
            cond.max_time_diff_to_upstream,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn component_override_enabled_true_with_all_none_thresholds_produces_no_backpressure() {
        // enabled=true (default) but all thresholds None → no backpressure for that component
        let config = BackpressureConfig {
            block_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(50),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            component_overrides: ComponentOverrides {
                tree_manager: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_diff_to_upstream: None,
                    max_time_diff_to_upstream: None,
                    max_batch_diff_to_upstream: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::TreeManager);
        assert!(cond.max_block_diff_to_upstream.is_none());
        assert!(cond.max_time_diff_to_upstream.is_none());
    }

    #[test]
    fn component_override_can_give_batch_component_stricter_threshold() {
        let config = BackpressureConfig {
            batch_pipeline: PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: Some(Duration::from_secs(300)),
                max_batch_diff_to_upstream: None,
            },
            component_overrides: ComponentOverrides {
                l1_sender_commit: Some(ComponentConditionOverride {
                    enabled: true,
                    max_block_diff_to_upstream: None,
                    max_time_diff_to_upstream: Some(Duration::from_secs(60)),
                    max_batch_diff_to_upstream: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let cond = config.condition_for(ComponentId::L1SenderCommit);
        assert_eq!(
            cond.max_time_diff_to_upstream,
            Some(Duration::from_secs(60))
        );

        // Others use group default
        let cond = config.condition_for(ComponentId::GaplessCommitter);
        assert_eq!(
            cond.max_time_diff_to_upstream,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn batch_pipeline_block_diff_to_upstream_applies_to_all_batch_components() {
        let config = BackpressureConfig {
            batch_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(200),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..Default::default()
        };
        for id in [
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
                cond.max_block_diff_to_upstream,
                Some(200),
                "batch component {id:?} must receive max_block_diff_to_upstream from PipelineCondition"
            );
        }
    }

    #[test]
    fn batch_pipeline_block_diff_to_upstream_does_not_affect_block_components() {
        let config = BackpressureConfig {
            batch_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(200),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
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
                cond.max_block_diff_to_upstream.is_none(),
                "block component {id:?} must not receive max_block_diff_to_upstream from batch_pipeline config"
            );
        }
    }

    #[test]
    fn batch_pipeline_default_block_diff_to_upstream_is_none() {
        let config = BackpressureConfig::default();
        let cond = config.condition_for(ComponentId::FriJobManager);
        assert!(cond.max_block_diff_to_upstream.is_none());
    }
}
