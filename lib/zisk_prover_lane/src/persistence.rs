//! Durable-persistence seam for the ZiSK aggregation lane.
//!
//! [`ZiskAggregationJobManager`](crate::ZiskAggregationJobManager) buffers per-batch
//! aggregation inputs and parked range proofs in memory. When a store is
//! attached it also writes them through, so a restart resumes the lane with its
//! GPU artifacts intact. The store itself (a bounded on-disk `ProofStorage`) is
//! shared with the Airbender FRI lane and lives in `node/bin`; the crate depends
//! only on this trait, never on the node binary. `node/bin`'s `ProofStorage`
//! implements it.

use crate::aggregation_job_manager::{AggregationInput, CompletedAggregatedProof};

/// The subset of the shared proof store the aggregation lane writes through.
/// Every method is best effort at the call site — a save failure never fails a
/// submission; persistence only adds restart durability.
#[async_trait::async_trait]
pub trait ZiskAggregationPersistence: Send + Sync {
    /// Save a buffered aggregation input for `batch_number`.
    async fn save_zisk_aggregation_input(
        &self,
        batch_number: u64,
        input: &AggregationInput,
    ) -> anyhow::Result<()>;

    /// Save a parked aggregated range proof for `[from_batch..=to_batch]`.
    async fn save_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: &CompletedAggregatedProof,
    ) -> anyhow::Result<()>;

    /// Delete a parked aggregated range proof for `[from_batch..=to_batch]`.
    async fn remove_zisk_aggregated_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
    ) -> anyhow::Result<()>;

    /// Drop persisted inputs and range proofs at or below `batch_to`.
    async fn prune_zisk_up_to(&self, batch_to: u64) -> anyhow::Result<()>;
}
