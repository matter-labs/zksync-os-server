use crate::batcher_metrics::{BATCHER_METRICS, BatchExecutionStage};
use crate::commitment::BatchInfo;
use alloy::primitives::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::time::SystemTime;
use time::UtcDateTime;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_observability::LatencyDistributionTracker;
// todo: these models are used throughout the batcher subsystem - not only l1 sender
//       we will move them to `types` or `batcher_types` when an analogous crate is created in `zksync-os`

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
    // This is not purely commitment information, but we keep old serialization name for
    // backwards-compatibility.
    #[serde(rename = "commit_batch_info")]
    pub batch_info: BatchInfo,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub tx_count: usize,
    #[serde(default = "default_execution_version")]
    pub execution_version: u32,
}

fn default_execution_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchEnvelope<E> {
    pub batch: BatchMetadata,
    pub data: E,
    #[serde(skip, default)]
    pub latency_tracker: LatencyDistributionTracker<BatchExecutionStage>,
}

impl<E> BatchEnvelope<E> {
    pub fn new(batch: BatchMetadata, data: E) -> Self {
        Self {
            batch,
            data,
            latency_tracker: LatencyDistributionTracker::default(),
        }
    }
    pub fn batch_number(&self) -> u64 {
        self.batch.batch_info.batch_number
    }
    pub fn time_since_first_block(&self) -> anyhow::Result<core::time::Duration> {
        let first_block_time = SystemTime::from(UtcDateTime::from_unix_timestamp(
            self.batch.batch_info.first_block_timestamp as i64,
        )?);

        Ok(SystemTime::now().duration_since(first_block_time)?)
    }

    // not 100% happy with this - `BatchEnvelope` shouldn't depend on metrics
    // maybe we can put metrics logic inside `LatencyDistributionTracker` generically,
    // but then it needs to have the batch_number as its field - which makes it non-generic.
    // On the other hand, we can treat the `BatchEnvelop` model as metrics/tracking-related
    //
    // Will be revisited on next `BatchEnvelope` iteration -
    // along with the fact that we almost always only use `BatchEnvelope<FriProof>`, so it being generic may be not justified

    pub fn set_stage(&mut self, stage: BatchExecutionStage) {
        let batch_number = self.batch_number();
        let last_block_number = self.batch.last_block_number;
        self.latency_tracker.record_stage(stage, |duration| {
            BATCHER_METRICS.execution_stages[&stage].observe(duration);
            BATCHER_METRICS.batch_number[&stage].set(batch_number);
            BATCHER_METRICS.block_number[&stage].set(last_block_number);
        });
    }

    pub fn with_stage(mut self, stage: BatchExecutionStage) -> BatchEnvelope<E> {
        self.set_stage(stage);
        self
    }

    pub fn with_data<N>(self, data: N) -> BatchEnvelope<N> {
        BatchEnvelope::<N> {
            batch: self.batch,
            data,
            latency_tracker: self.latency_tracker,
        }
    }
}

pub type ProverInput = Vec<u32>;

#[derive(Clone, Serialize, Deserialize)]
pub enum FriProof {
    // Fake proof for testing purposes
    Fake,
    Real(RealFriProof),
}

// V1 can be dropped if there testnet-alpha will be regenerated from scratch.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RealFriProof {
    V1(Bytes),
    V2 {
        proof: Bytes,
        proving_execution_version: u32,
    },
}

impl FriProof {
    pub fn is_fake(&self) -> bool {
        matches!(self, FriProof::Fake)
    }

    pub fn proving_execution_version(&self) -> Option<u32> {
        match self {
            FriProof::Real(RealFriProof::V2 {
                proving_execution_version,
                ..
            }) => Some(*proving_execution_version),
            _ => None,
        }
    }

    pub fn proof(&self) -> Option<&[u8]> {
        match self {
            FriProof::Real(real) => Some(real.proof()),
            FriProof::Fake => None,
        }
    }
}

impl RealFriProof {
    pub fn proof(&self) -> &[u8] {
        match self {
            RealFriProof::V1(proof) => proof.as_ref(),
            RealFriProof::V2 { proof, .. } => proof.as_ref(),
        }
    }
}

impl Debug for FriProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FriProof::Fake => write!(f, "Fake"),
            FriProof::Real(_) => write!(
                f,
                "Real(proving_execution_version={:?}, len: {:?})",
                self.proving_execution_version(),
                self.proof().unwrap().len()
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SnarkProof {
    // Fake proof for testing purposes
    Fake,
    Real(RealSnarkProof),
}

// V1 can be dropped if there testnet-alpha will be regenerated from scratch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RealSnarkProof {
    V1(Vec<u8>),
    V2 {
        proof: Vec<u8>,
        proving_execution_version: u32,
    },
}

impl SnarkProof {
    pub fn proving_execution_version(&self) -> Option<u32> {
        match self {
            SnarkProof::Real(RealSnarkProof::V2 {
                proving_execution_version,
                ..
            }) => Some(*proving_execution_version),
            _ => None,
        }
    }

    pub fn proof(&self) -> Option<&[u8]> {
        match self {
            SnarkProof::Real(real) => Some(real.proof()),
            SnarkProof::Fake => None,
        }
    }
}

impl RealSnarkProof {
    pub fn proof(&self) -> &[u8] {
        match self {
            RealSnarkProof::V1(proof) => proof.as_slice(),
            RealSnarkProof::V2 { proof, .. } => proof.as_slice(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v1_proof_deserialization() {
        // Real testnet envelope. Proof was shortened for brevity.
        let data = r#"{"batch":{"previous_stored_batch_info":{"batch_number":9,"state_commitment":"0x7e7f4bbd2fac4431253feccd4688d4b060d720c9cdb5eb06267e9cc8fdfad39d","number_of_layer1_txs":0,"priority_operations_hash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","dependency_roots_rolling_hash":"0x0000000000000000000000000000000000000000000000000000000000000000","l2_to_l1_logs_root_hash":"0x692f35c99f9c698852289ffecf07f6dd45770904521149d79aa85aae598fa375","commitment":"0xf1dfa8fe5d6571e1c9bdb01f574cff0cbe8c23183c4fcd6d7dd1b4128e54287c","last_block_timestamp":1758115458},"commit_batch_info":{"batch_number":10,"new_state_commitment":"0x53680ad464b20f43921708bd3e024f365b788b9e11cf49e783607a42172136fc","number_of_layer1_txs":0,"priority_operations_hash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","dependency_roots_rolling_hash":"0x0000000000000000000000000000000000000000000000000000000000000000","l2_to_l1_logs_root_hash":"0x692f35c99f9c698852289ffecf07f6dd45770904521149d79aa85aae598fa375","l2_da_validator":"0x0000000000000000000000000000000000000000","da_commitment":"0x86b130c978627d2acb4a68c823cfc31efadf6482862566d364cc4bc15e500e2b","first_block_timestamp":1758116549,"last_block_timestamp":1758116549,"chain_id":8022833,"chain_address":"0x02b1ac1cf0a592aefd3c2246b2431388365db272","operator_da_input":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,201,102,180,205,111,127,203,19,178,222,176,220,147,85,249,171,106,46,88,99,189,117,148,44,88,11,167,49,72,205,72,21,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,116,25,135,1,193,217,21,41,206,115,57,17,55,153,69,34,75,25,41,48,9,20,117,70,62,143,98,164,122,16,216,160,0,0,0,2,193,25,138,114,80,95,70,215,34,237,142,12,160,249,191,228,43,163,162,216,104,166,24,217,213,90,128,186,146,85,247,97,20,33,1,64,111,64,166,72,80,155,187,230,197,73,156,145,87,2,137,219,217,151,57,45,241,113,145,154,157,86,109,62,141,1,57,228,183,230,28,9,1,34,1,64,111,64,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"upgrade_tx_hash":null},"first_block_number":10,"last_block_number":10,"tx_count":1,"execution_version":1},"data":{"Real":[2,252,54,244]}}"#;
        let b = serde_json::from_str::<BatchEnvelope<FriProof>>(data).unwrap();
        assert!(matches!(b.data, FriProof::Real(RealFriProof::V1(_))));
    }
}
