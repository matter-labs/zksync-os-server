//! The ZiSK proving lane: the server-side job managers, aggregation,
//! commitment, metrics and multi-proof composition for the second proof
//! system. Transport lives at the binary edge, beside the Airbender handlers.
//!
//! Batches enter the lane at seal time (a per-batch ZiSK job is created
//! alongside the Airbender FRI job), external provers pick and submit through
//! the HTTP handlers, and the aggregation stage collapses each Airbender SNARK
//! range into one range proof that the multi-proof rendezvous pairs with the
//! Airbender range SNARK ([`compose_multiproof`]).
//!
//! The crate depends only on other lib crates and the guest / verifier
//! libraries; it has no tie to the server binary. The rendezvous orchestration
//! that reads the `SnarkJobManager` job map, and the axum route registration,
//! stay in `node/bin` and call into this crate.

mod aggregation_job_manager;
mod bytes;
mod combine;
mod commitment;
mod job_manager;
mod metrics;
mod proving_version;
mod range;
mod shadow;
mod vadcop_stream;

#[cfg(test)]
mod test_util;

pub use aggregation_job_manager::{
    AggregationInput, AggregationInputOutcome, CompletedAggregatedProof, ZiskAggregationCounts,
    ZiskAggregationJob, ZiskAggregationJobManager, ZiskAggregationRangeStatus,
    ZiskAggregationSubmitError,
};
pub use aggregation_job_manager::{ZiskAggregationLaneConfig, ZiskAggregationMode};
pub use bytes::ZiskBatchBytes;
pub use combine::compose_multiproof;
pub use commitment::{
    ZISK_COMMITTED_VALUE_RANGE, ZISK_PUBLIC_VALUES_BYTES, committed_value,
    expected_zisk_public_input,
};
pub use job_manager::MAX_TOTAL_JOBS;
pub use job_manager::{
    MultiProofMode, ZiskBatchStatus, ZiskJob, ZiskJobData, ZiskJobManager, ZiskQueueCounts,
    ZiskSubmitError, ZiskVkSet,
};
pub use job_manager::{ZiskLaneConfig, ZiskLaneMode, ZiskLaneWiring};
pub use metrics::ZISK_LANE_METRICS;
pub use proving_version::{
    ZiskProvingVersion, ZiskProvingVersionError, ZiskReleaseManifest, ZiskVersionKeys,
};
pub use range::{BatchRange, InvalidBatchRange};
pub use shadow::shadow_execute_zisk_batch;

/// Test-only wire fixtures, exposed for tests in other crates via the
/// `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub use commitment::synthetic_public_values;
#[cfg(any(test, feature = "test-support"))]
pub use vadcop_stream::synthetic_stream;

/// Batch-level ZiSK witness builds attempted so far. The disabled-mode
/// differential test asserts this never moves while the lane is off.
pub fn batch_witness_attempts() -> u64 {
    crate::metrics::ZISK_LANE_METRICS
        .batch_witness_attempts
        .get()
}

/// Record that a batch-level ZiSK witness build was attempted.
pub fn count_batch_witness_attempt() {
    crate::metrics::ZISK_LANE_METRICS
        .batch_witness_attempts
        .inc();
}
