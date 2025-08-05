use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::time::SystemTime;
use time::{OffsetDateTime, UtcDateTime};
use tokio::time::Instant;

// todo: these models are used throughout the batcher subsystem - not only l1 sender
//       we should move them to a separate crate (`types`?)

/// Information about a batch that is enough for all L1 operations.
/// Used throughout the batcher subsystem
/// We may want to rework it -
///    instead of putting computed CommitBatchInfo/StoredBatchInfo here (L1 contract-specific classes),
///    we may want to include lower-level fields
///
///  Note that any change to this struct is breaking since we serialize it in `ProofStorage`
///
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchMetadata {
    pub previous_stored_batch_info: StoredBatchInfo,
    pub commit_batch_info: CommitBatchInfo,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub tx_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchEnvelope<E> {
    pub batch: BatchMetadata,
    pub data: E,
    #[serde(skip, default)]
    pub trace: Trace,
}

impl<A> BatchEnvelope<A> {
    pub fn batch_number(&self) -> u64 {
        self.batch.commit_batch_info.batch_number
    }
    pub fn time_since_first_block(&self) -> anyhow::Result<core::time::Duration> {
        let first_block_time = SystemTime::from(UtcDateTime::from_unix_timestamp(
            self.batch.commit_batch_info.first_block_timestamp as i64,
        )?);

        Ok(SystemTime::now().duration_since(first_block_time)?)
    }
    pub fn with_trace_stage(mut self, stage: &'static str) -> Self {
        self.trace = self.trace.with_stage(stage);
        self
    }
}

/// Trace of the batch processing - has timestamps of each stage the batch went through
/// Do not use it for business logic
/// (although currently used to determine when to give up on waiting for a prover and use fake proof instead)
#[derive(Clone, Debug)]
pub struct Trace {
    pub start_time: SystemTime,
    pub start_instant: Instant,
    pub stages: Vec<(&'static str, Instant)>,
}

pub type ProverInput = Vec<u32>;

#[derive(Clone, Serialize, Deserialize)]
pub enum FriProof {
    // Fake proof for testing purposes
    Fake,
    Real(Vec<u8>),
}

impl FriProof {
    pub fn is_fake(&self) -> bool {
        matches!(self, FriProof::Fake)
    }
}

impl Debug for FriProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FriProof::Fake => write!(f, "Fake"),
            FriProof::Real(proof) => write!(f, "Real(len: {:?})", proof.len()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SnarkProof {
    // Fake proof for testing purposes
    Fake,
    Real(Vec<u8>),
}

impl Trace {
    pub fn with_stage(mut self, stage: &'static str) -> Trace {
        self.stages.push((stage, Instant::now()));
        self
    }
    pub fn last_stage_age(&self) -> std::time::Duration {
        self.stages
            .last()
            .map(|(_, instant)| instant.elapsed())
            .unwrap_or(self.start_instant.elapsed())
    }
}

impl Display for Trace {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "start: {}", fmt_ts(self.start_time))?;
        write!(
            f,
            "; total: {:?} (",
            Instant::now().duration_since(self.start_instant)
        )?;

        let mut prev = self.start_instant;
        for (name, ts) in &self.stages {
            let delta = ts.duration_since(prev);
            write!(f, "{name}: +{delta:?} ")?;
            prev = *ts;
        }
        write!(f, ")")?;
        Ok(())
    }
}
fn fmt_ts(ts: SystemTime) -> String {
    let odt = OffsetDateTime::from(ts);
    odt.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            start_time: SystemTime::now(),
            start_instant: Instant::now(),
            stages: vec![],
        }
    }
}
