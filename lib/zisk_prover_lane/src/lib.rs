//! The ZiSK proving lane: the server-side job managers, aggregation,
//! commitment, metrics, multi-proof composition and HTTP handler bodies for the
//! second proof system.
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
//! stay in `node/bin` and call into this crate. The shared on-disk proof store
//! also stays in `node/bin`; the aggregation lane writes through it via the
//! [`ZiskAggregationPersistence`] trait.

mod aggregation_job_manager;
mod bytes;
mod combine;
mod commitment;
pub mod handlers;
mod job_manager;
mod metrics;
mod persistence;
mod shadow;
mod vadcop_stream;

#[cfg(test)]
mod test_util;

pub use aggregation_job_manager::{
    AggregationInput, AggregationInputOutcome, CompletedAggregatedProof, ZiskAggregationCounts,
    ZiskAggregationJob, ZiskAggregationJobManager, ZiskAggregationRangeStatus,
    ZiskAggregationSubmitError, expected_aggregated_public_input,
};
pub use bytes::ZiskBatchBytes;
pub use combine::compose_multiproof;
pub use commitment::{ZISK_PUBLIC_VALUES_BYTES, expected_zisk_public_input};
pub use job_manager::{
    MultiProofMode, ZiskBatchStatus, ZiskJob, ZiskJobData, ZiskJobManager, ZiskQueueCounts,
    ZiskSubmitError, ZiskVkSet,
};
pub use metrics::ZISK_LANE_METRICS;
pub use persistence::ZiskAggregationPersistence;
pub use shadow::shadow_execute_zisk_batch;

/// Test-only proof-stream fixture, exposed for tests in other crates via the
/// `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub use vadcop_stream::synthetic_stream;
