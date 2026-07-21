//! Consensus-side node metrics: what this validator has committed, what it holds
//! speculatively, and how it judges proposals. The consensus engine's own internals
//! (views, votes, network) are covered separately by the commonware runtime's
//! registry; these metrics cover the execution seam, where consensus meets the node.

use vise::{Counter, Gauge, Global, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "consensus")]
pub struct ConsensusMetrics {
    /// Height of the last block committed to the durable chain.
    pub committed_height: Gauge<u64>,
    /// Unix seconds of the last commit. `now - this` is the commit-lag alert signal:
    /// on a healthy chain it stays within a few block times.
    pub last_commit_unix: Gauge<u64>,
    /// Blocks currently held speculatively (verified or built, not yet committed).
    /// Sustained growth means commits lag finalization; the hard cap turns that into
    /// withheld votes.
    pub speculative_blocks: Gauge<u64>,
    /// Verification verdicts on leader proposals, by outcome: `valid`, `invalid`
    /// (permanent — a byzantine-leader alert when it fires repeatedly), `withhold`
    /// (cannot vouch *yet* — routine under L1 lag or after restarts).
    #[metrics(labels = ["verdict"])]
    pub verify_verdicts: LabeledFamily<&'static str, Counter>,
    /// Leader-turn build outcomes: `built` or `passed` (declined to propose).
    #[metrics(labels = ["outcome"])]
    pub build_outcomes: LabeledFamily<&'static str, Counter>,
    /// Committee transaction gossip, by direction and fate: `sent` (batches out),
    /// `received` (batches in), `admitted` (transactions entering the pool),
    /// `ignored` (duplicates and pool rejections — routine), `undecodable`
    /// (malformed input from an authenticated peer — worth eyes).
    #[metrics(labels = ["event"])]
    pub tx_gossip: LabeledFamily<&'static str, Counter>,
    /// Consensus activity observed by this validator, by kind — `finalization`,
    /// `notarization`, and the byzantine evidence kinds (`conflicting_notarize`,
    /// `conflicting_finalize`, `nullify_finalize`), which must stay at zero on a
    /// healthy committee and are the fault-evidence alert signal.
    #[metrics(labels = ["kind"])]
    pub activity: LabeledFamily<&'static str, Counter>,
}

#[vise::register]
pub static CONSENSUS_METRICS: Global<ConsensusMetrics> = Global::new();
