//! What a leader does with its turn when the mempool is empty.
//!
//! This module is a deliberate, self-contained strategy — the whole "idle
//! chain" behavior lives here, and the builder consults it at exactly one
//! point (its idle branch). To remove the strategy, delete this module and
//! make that branch unconditionally build (the pre-policy behavior, which
//! [`IdlePolicy::legacy`] still provides).
//!
//! The strategy, in order of precedence:
//!
//! 1. **Sprint to activation.** While the committee schedule contains an entry
//!    that has not activated yet, idle leaders keep building empty blocks at
//!    full cadence. Epochs are height-driven, so a scheduled committee change
//!    on an idle chain would otherwise wait one heartbeat per block of
//!    distance to its epoch boundary; sprinting bounds
//!    activation latency by `epoch_length / block rate` regardless of
//!    traffic — this is what makes rotating a validator on a quiet chain
//!    fast. Self-limiting: once the boundary passes, normal idle rules
//!    resume.
//! 2. **Heartbeat.** If the parent block is older than the configured
//!    interval, build one empty block. This bounds everything that anchors to
//!    finalized progress — vote-journal pruning, fee-clamp staleness, the
//!    batcher's settlement cadence — and doubles as the liveness probe: on a
//!    healthy chain "no block for longer than the interval plus a margin" is
//!    always an alarm.
//! 3. **Decline.** Otherwise, pass the turn: no block. Consensus nullifies
//!    the view and rotates leaders; the next transaction (or relayed L1
//!    priority operation) produces a block within a leader timeout or two.
//!
//! Declining and building empty are both always safe — empty blocks are legal
//! under verification, and a declined turn is just a nullified view — so
//! validators with differing configurations (or none of this policy at all)
//! interoperate freely; the policy only shapes when blocks get made.

use std::num::NonZeroU64;
use std::time::Duration;

/// Decides whether an idle leader turn produces an empty block.
#[derive(Debug, Clone)]
pub struct IdlePolicy {
    mode: Mode,
}

#[derive(Debug, Clone)]
enum Mode {
    /// The pre-policy behavior: an idle leader always builds, so the chain
    /// seals empty blocks on the builder's cadence around the clock.
    AlwaysBuild,
    Heartbeat {
        interval: Duration,
        epoch_length: NonZeroU64,
        /// Activation epochs from the committee schedule; the sprint targets.
        /// Entries at or below the current epoch are already active and inert.
        activation_epochs: Vec<u64>,
    },
}

/// The verdict for one idle leader turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDecision {
    /// Pass the turn: no block this view.
    Decline,
    /// Build the empty block anyway, for this reason (logged for operators).
    BuildEmpty(EmptyBlockReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyBlockReason {
    /// The policy is disabled; idle leaders always build.
    LegacyCadence,
    /// The parent is older than the heartbeat interval.
    Heartbeat,
    /// A scheduled committee change is waiting for its epoch boundary.
    SprintToActivation,
}

impl IdlePolicy {
    /// The pre-policy behavior: idle leaders always build (constant cadence).
    pub fn legacy() -> Self {
        Self {
            mode: Mode::AlwaysBuild,
        }
    }

    /// Heartbeat mode: decline idle turns unless the parent is older than
    /// `interval` or a schedule entry is still waiting to activate.
    pub fn heartbeat(
        interval: Duration,
        epoch_length: NonZeroU64,
        activation_epochs: Vec<u64>,
    ) -> Self {
        Self {
            mode: Mode::Heartbeat {
                interval,
                epoch_length,
                activation_epochs,
            },
        }
    }

    /// The verdict for building block `parent_number + 1` at `now` (unix
    /// seconds, the proposer's clock). Pure: every input is chain data or
    /// static configuration, so the decision is cheap and unit-testable, and
    /// a leader needs no channels or shared state to make it.
    pub fn decide(&self, parent_number: u64, parent_timestamp: u64, now: u64) -> IdleDecision {
        match &self.mode {
            Mode::AlwaysBuild => IdleDecision::BuildEmpty(EmptyBlockReason::LegacyCadence),
            Mode::Heartbeat {
                interval,
                epoch_length,
                activation_epochs,
            } => {
                let next_epoch = (parent_number + 1) / epoch_length.get();
                if activation_epochs.iter().any(|epoch| *epoch > next_epoch) {
                    return IdleDecision::BuildEmpty(EmptyBlockReason::SprintToActivation);
                }
                if now.saturating_sub(parent_timestamp) >= interval.as_secs() {
                    return IdleDecision::BuildEmpty(EmptyBlockReason::Heartbeat);
                }
                IdleDecision::Decline
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(interval_secs: u64, epoch_length: u64, activations: &[u64]) -> IdlePolicy {
        IdlePolicy::heartbeat(
            Duration::from_secs(interval_secs),
            NonZeroU64::new(epoch_length).unwrap(),
            activations.to_vec(),
        )
    }

    #[test]
    fn legacy_always_builds() {
        assert_eq!(
            IdlePolicy::legacy().decide(5, 1_000, 1_000),
            IdleDecision::BuildEmpty(EmptyBlockReason::LegacyCadence),
        );
    }

    #[test]
    fn fresh_parent_declines_and_stale_parent_heartbeats() {
        let policy = policy(600, 100, &[]);
        assert_eq!(policy.decide(5, 1_000, 1_599), IdleDecision::Decline);
        assert_eq!(
            policy.decide(5, 1_000, 1_600),
            IdleDecision::BuildEmpty(EmptyBlockReason::Heartbeat),
        );
        // A parent timestamp ahead of the local clock (peer skew) reads as
        // fresh, not as a huge negative age.
        assert_eq!(policy.decide(5, 2_000, 1_000), IdleDecision::Decline);
    }

    #[test]
    fn a_pending_activation_sprints_until_its_boundary_passes() {
        // Epoch length 100, entry activating at epoch 2 (height 200).
        let policy = policy(600, 100, &[2]);
        // Fresh parent, but the boundary is ahead: sprint.
        assert_eq!(
            policy.decide(150, 1_000, 1_001),
            IdleDecision::BuildEmpty(EmptyBlockReason::SprintToActivation),
        );
        // Next block is 200 = first height of epoch 2: the entry is active;
        // normal idle rules resume immediately.
        assert_eq!(policy.decide(199, 1_000, 1_001), IdleDecision::Decline);
        // Long-past entries stay inert.
        assert_eq!(policy.decide(950, 1_000, 1_001), IdleDecision::Decline);
    }

    #[test]
    fn sprint_takes_precedence_over_heartbeat_bookkeeping() {
        // Both conditions hold; the reason reported is the sprint (operators
        // should see why the chain is suddenly producing at full cadence).
        let policy = policy(600, 100, &[5]);
        assert_eq!(
            policy.decide(10, 1_000, 5_000),
            IdleDecision::BuildEmpty(EmptyBlockReason::SprintToActivation),
        );
    }
}
