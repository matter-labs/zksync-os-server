use crate::{BatchSignatureSet, PendingBatchInfo};
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Address, B256, Bytes};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::time::SystemTime;
use time::UtcDateTime;
use zksync_os_batcher_metrics::{BATCHER_METRICS, BatchExecutionStage};
use zksync_os_contract_interface::models::{L2Log, StoredBatchInfo};
use zksync_os_observability::LatencyDistributionTracker;
use zksync_os_pipeline::HasBlockRangeEnd;
use zksync_os_types::{ProvingStackConfiguration, PubdataMode, require_proving_config};

/// Information about a batch that is enough for all L1 operations.
/// Used throughout the batcher subsystem
/// We may want to rework it -
///    instead of putting computed CommitBatchInfo/StoredBatchInfo here (L1 contract-specific classes),
///    we may want to include lower-level fields
///
///  Note that we serialize it in `ProofStorage`, so a change here will invalidate old entries
///  This isn't really a problem as we only store the recent ones
///
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchMetadata {
    pub previous_stored_batch_info: StoredBatchInfo,
    // This is not purely commitment information, but we keep old serialization name for
    // backwards-compatibility.
    #[serde(rename = "commit_batch_info")]
    pub batch_info: PendingBatchInfo,
    pub chain_address: Address,
    pub blob_sidecar: Option<BlobTransactionSidecar>,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub last_block_hash: Option<B256>,
    #[serde(default = "default_pubdata_mode")]
    pub pubdata_mode: PubdataMode,
    // note: can equal to zero
    pub tx_count: usize,
    #[serde(default)]
    pub computational_native_used: Option<u64>,
    #[serde(default)]
    pub logs: Vec<L2Log>,
    #[serde(default)]
    pub messages: Vec<Vec<u8>>,
    #[serde(default)]
    pub multichain_root: B256,
    /// Migration number of the `SetSLChainId` system transaction executed in this batch, if any.
    /// `None` for the vast majority of batches; `Some(n)` only for the single batch that contains
    /// the `SetSLChainId` transaction triggered by a gateway migration.
    #[serde(default)]
    pub set_sl_chain_id_migration_number: Option<u64>,
}

impl BatchMetadata {
    /// Gets batch metadata verification key hash.
    pub fn verification_key_hash(&self) -> anyhow::Result<&'static str> {
        Ok(self.proving_config()?.verification_key_hash)
    }

    pub fn proving_config(&self) -> anyhow::Result<&'static ProvingStackConfiguration> {
        require_proving_config(&self.batch_info.protocol_version, "batch metadata access")
            .context("Failed to get proving configuration from protocol version")
    }
}

fn default_pubdata_mode() -> PubdataMode {
    PubdataMode::Calldata
}

#[derive(Debug)]
pub struct MissingSignature;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub enum BatchSignatureData {
    Signed {
        signatures: BatchSignatureSet,
    },
    /// Batch was already committed, but is going through pipeline the second time.
    /// We do not need to have signatures for it now
    AlreadyCommitted,
    // default to allow deserializing of older objects
    /// Batch signatures are not enabled
    #[default]
    NotNeeded,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchEnvelope<E, S> {
    pub batch: BatchMetadata,
    pub data: E,
    #[serde(default)] // to allow deserializing older objects
    pub signature_data: S,
    #[serde(skip, default)]
    pub latency_tracker: LatencyDistributionTracker<BatchExecutionStage>,
}

pub type BatchForSigning<E> = BatchEnvelope<E, MissingSignature>;
pub type SignedBatchEnvelope<E> = BatchEnvelope<E, BatchSignatureData>;

impl<E> BatchEnvelope<E, MissingSignature> {
    pub fn new(batch: BatchMetadata, data: E) -> Self {
        Self {
            batch,
            data,
            signature_data: MissingSignature,
            latency_tracker: LatencyDistributionTracker::default(),
        }
    }

    pub fn with_signatures(
        self,
        signature_data: BatchSignatureData,
    ) -> BatchEnvelope<E, BatchSignatureData> {
        BatchEnvelope {
            batch: self.batch,
            data: self.data,
            signature_data,
            latency_tracker: self.latency_tracker,
        }
    }
}

impl<E, S> BatchEnvelope<E, S> {
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
            if !matches!(
                stage,
                BatchExecutionStage::CommitL1Passthrough
                    | BatchExecutionStage::ProveL1Passthrough
                    | BatchExecutionStage::ExecuteL1Passthrough
            ) {
                BATCHER_METRICS.batch_number[&stage].set(batch_number);
                BATCHER_METRICS.block_number[&stage].set(last_block_number);
            }
        });
    }

    pub fn with_stage(mut self, stage: BatchExecutionStage) -> BatchEnvelope<E, S> {
        self.set_stage(stage);
        self
    }

    pub fn with_data<N>(self, data: N) -> BatchEnvelope<N, S> {
        BatchEnvelope {
            batch: self.batch,
            data,
            signature_data: self.signature_data,
            latency_tracker: self.latency_tracker,
        }
    }
}

/// Input data required to generate a ZK proof for a batch.
///
/// Used for tests and testnets where the expensive RiscV witness computation is unnecessary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProverInput {
    Real(Vec<u32>),
    Fake,
}

impl ProverInput {
    /// Returns the underlying witness words.
    /// Panics if called on `Fake`.
    pub fn unwrap_real(&self) -> &[u32] {
        match self {
            ProverInput::Real(v) => v.as_slice(),
            ProverInput::Fake => panic!("ProverInput::Fake has no witness data"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum FriProof {
    // Fake proof for testing purposes
    Fake,
    // Marker for batches that were already proven on L1, so we don't need to prove them again
    AlreadySubmittedToL1,
    Real(RealFriProof),
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum RealFriProof {
    V2 { proof: Bytes },
}

#[derive(Deserialize)]
struct RealFriProofRepr {
    proof: Bytes,
    #[serde(default, rename = "proving_execution_version")]
    _legacy_proving_ordinal: Option<serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for RealFriProof {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = RealFriProofRepr::deserialize(deserializer)?;
        Ok(Self::V2 { proof: repr.proof })
    }
}

impl FriProof {
    pub fn is_fake(&self) -> bool {
        matches!(self, FriProof::Fake)
    }

    pub fn proof(&self) -> Option<&[u8]> {
        match self {
            FriProof::Real(real) => Some(real.proof()),
            FriProof::Fake | FriProof::AlreadySubmittedToL1 => None,
        }
    }
}

impl RealFriProof {
    pub fn proof(&self) -> &[u8] {
        match self {
            RealFriProof::V2 { proof, .. } => proof.as_ref(),
        }
    }
}

impl Debug for FriProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FriProof::Fake => write!(f, "Fake"),
            FriProof::AlreadySubmittedToL1 => write!(f, "AlreadySubmittedToL1"),
            FriProof::Real(_) => write!(f, "Real(len: {:?})", self.proof().unwrap().len()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SnarkProof {
    // Fake proof for testing purposes
    Fake,
    Real(RealSnarkProof),
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RealSnarkProof {
    V2 { proof: Vec<u8> },
}

#[derive(Deserialize)]
struct RealSnarkProofRepr {
    proof: Vec<u8>,
    #[serde(default, rename = "proving_execution_version")]
    _legacy_proving_ordinal: Option<serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for RealSnarkProof {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = RealSnarkProofRepr::deserialize(deserializer)?;
        Ok(Self::V2 { proof: repr.proof })
    }
}

impl SnarkProof {
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
            RealSnarkProof::V2 { proof, .. } => proof.as_slice(),
        }
    }
}

impl<E: Send + 'static, S: Send + 'static> HasBlockRangeEnd for BatchEnvelope<E, S> {
    fn block_number(&self) -> u64 {
        self.batch.last_block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.batch.batch_info.last_block_timestamp)
    }
    fn batch_number(&self) -> Option<u64> {
        Some(self.batch.batch_info.batch_number)
    }
}

#[cfg(test)]
mod tests {
    use super::{RealFriProof, RealSnarkProof};
    use alloy::primitives::Bytes;

    #[test]
    fn new_proof_envelopes_omit_the_legacy_proving_ordinal() {
        let fri = RealFriProof::V2 {
            proof: Bytes::from(vec![1, 2, 3]),
        };
        let snark = RealSnarkProof::V2 {
            proof: vec![4, 5, 6],
        };

        for value in [
            serde_json::to_value(fri).unwrap(),
            serde_json::to_value(snark).unwrap(),
        ] {
            assert!(value.get("proving_execution_version").is_none());
        }
    }

    #[test]
    fn legacy_proof_ordinals_are_ignored_while_decoding() {
        let fri: RealFriProof = serde_json::from_value(serde_json::json!({
            "proof": "0x010203",
            "proving_execution_version": 7,
        }))
        .unwrap();
        let snark: RealSnarkProof = serde_json::from_value(serde_json::json!({
            "proof": [4, 5, 6],
            "proving_execution_version": 8,
        }))
        .unwrap();

        assert_eq!(fri.proof(), &[1, 2, 3]);
        assert_eq!(snark.proof(), &[4, 5, 6]);
    }
}
