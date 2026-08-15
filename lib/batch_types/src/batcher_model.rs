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
use zksync_os_types::{ProvingVersion, PubdataMode};

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
        Ok(
            ProvingVersion::try_from(self.batch_info.protocol_version.clone())
                .context("Failed to get proving version from protocol version")?
                .vk_hash(),
        )
    }

    pub fn proving_version(&self) -> anyhow::Result<ProvingVersion> {
        Ok(ProvingVersion::try_from(
            self.batch_info.protocol_version.clone(),
        )?)
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

/// Everything a sealed batch carries for the proof systems that will prove it.
///
/// The primary lane's witness has always ridden the envelope; the second proof
/// system's does too, so the batcher hands its products to the pipeline rather
/// than reaching into a proving lane at seal time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvingInputs {
    /// The Airbender witness.
    pub fri: ProverInput,
    /// The second proof system's input, when that system is on. `None` keeps
    /// the envelope byte-identical to a single-proof node's.
    pub second_proof: Option<SecondProofInput>,
}

/// What the seal produced for the second proof system.
///
/// The commitment only means anything alongside the input it was computed
/// from, so the two travel together rather than as two independent options
/// that could disagree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecondProofInput {
    /// Serialized batch input for the second proof system's guest.
    pub bytes: Vec<u8>,
    /// The guest commitment from seal-time shadow execution, if it ran: the
    /// local arbiter a later submission is judged against.
    pub seal_commitment: Option<alloy::primitives::B256>,
}

impl ProvingInputs {
    /// A batch with no second proof system configured.
    pub fn fri_only(fri: ProverInput) -> Self {
        Self {
            fri,
            second_proof: None,
        }
    }
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
            FriProof::Fake | FriProof::AlreadySubmittedToL1 => None,
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
            FriProof::AlreadySubmittedToL1 => write!(f, "AlreadySubmittedToL1"),
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
    /// Multi-proof: Airbender SNARK + ZiSK SNARK verified together on-chain.
    MultiProof(MultiProofSnarkProof),
}

/// ZiSK SNARK proof size: 24 BN254 points × 32 bytes = 768 bytes.
pub const ZISK_SNARK_PROOF_BYTES: usize = 768;

/// Combined proof for the multi-proof system (Airbender + ZiSK).
///
/// Both proof systems must independently verify the same batch state transition.
/// The `MultiProofVerifier` L1 contract rejects the proof if either fails.
///
/// Proof encoding on L1 (type 5):
/// `[type|version, prevHash, N, airbender[N], zisk[24]]`
///
/// The type-5 payload carries the SNARK words only. The on-chain
/// MultiProofVerifier reconstructs the ZiSK public values from its pinned VKs
/// and the batch public inputs. The aggregation manager validates the binding
/// digest off-chain before submission.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "MultiProofSnarkProofWire")]
pub struct MultiProofSnarkProof {
    /// Airbender SNARK proof bytes (Plonk format, multiple of 32 bytes).
    airbender_proof: Vec<u8>,
    /// ZiSK SNARK proof bytes (768 bytes = 24 BN254 points).
    zisk_proof: Vec<u8>,
    /// Proving execution version for verifier routing.
    proving_execution_version: u32,
}

/// The wire form, with the same field names and order as the type itself, so
/// the serialized representation is unchanged. Deserialization goes through it
/// and then through `new`, because `derive(Deserialize)` would write the
/// private fields directly and hand back a value the constructor would have
/// refused.
#[derive(Deserialize)]
struct MultiProofSnarkProofWire {
    airbender_proof: Vec<u8>,
    zisk_proof: Vec<u8>,
    proving_execution_version: u32,
}

impl TryFrom<MultiProofSnarkProofWire> for MultiProofSnarkProof {
    type Error = MultiProofShapeError;

    fn try_from(wire: MultiProofSnarkProofWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.airbender_proof,
            wire.zisk_proof,
            wire.proving_execution_version,
        )
    }
}

/// A pair of proofs that cannot be a MultiProof.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MultiProofShapeError {
    #[error("ZiSK proof is {got} bytes, expected {expected}")]
    ZiskProofSize { got: usize, expected: usize },
    #[error("Airbender proof is {len} bytes, which is not a multiple of 32")]
    AirbenderProofNotAligned { len: usize },
}

impl MultiProofSnarkProof {
    /// Both shapes are invariants of the on-chain verifiers, so they are
    /// checked once here — where the two proofs are first put together —
    /// rather than at the L1 encoder, several stages later.
    pub fn new(
        airbender_proof: Vec<u8>,
        zisk_proof: Vec<u8>,
        proving_execution_version: u32,
    ) -> Result<Self, MultiProofShapeError> {
        if zisk_proof.len() != ZISK_SNARK_PROOF_BYTES {
            return Err(MultiProofShapeError::ZiskProofSize {
                got: zisk_proof.len(),
                expected: ZISK_SNARK_PROOF_BYTES,
            });
        }
        if !airbender_proof.len().is_multiple_of(32) {
            return Err(MultiProofShapeError::AirbenderProofNotAligned {
                len: airbender_proof.len(),
            });
        }
        Ok(Self {
            airbender_proof,
            zisk_proof,
            proving_execution_version,
        })
    }

    pub fn airbender_proof(&self) -> &[u8] {
        &self.airbender_proof
    }

    pub fn zisk_proof(&self) -> &[u8] {
        &self.zisk_proof
    }

    pub fn proving_execution_version(&self) -> u32 {
        self.proving_execution_version
    }
}

impl std::fmt::Debug for MultiProofSnarkProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiProofSnarkProof")
            .field("airbender_proof_len", &self.airbender_proof.len())
            .field("zisk_proof_len", &self.zisk_proof.len())
            .field("proving_execution_version", &self.proving_execution_version)
            .finish()
    }
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
            SnarkProof::MultiProof(two) => Some(two.proving_execution_version()),
            _ => None,
        }
    }

    /// The Airbender portion. A MultiProof carries two proofs; this is the one
    /// the Airbender verifier reads, and the name says so.
    pub fn airbender_proof(&self) -> Option<&[u8]> {
        match self {
            SnarkProof::Real(real) => Some(real.proof()),
            SnarkProof::MultiProof(two) => Some(two.airbender_proof()),
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
    use super::*;

    #[test]
    fn test_v1_proof_deserialization() {
        // Real testnet envelope. Proof was shortened for brevity.
        let data = r#"{"batch":{"previous_stored_batch_info":{"batch_number":9,"state_commitment":"0x7e7f4bbd2fac4431253feccd4688d4b060d720c9cdb5eb06267e9cc8fdfad39d","number_of_layer1_txs":0,"priority_operations_hash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","dependency_roots_rolling_hash":"0x0000000000000000000000000000000000000000000000000000000000000000","l2_to_l1_logs_root_hash":"0x692f35c99f9c698852289ffecf07f6dd45770904521149d79aa85aae598fa375","commitment":"0xf1dfa8fe5d6571e1c9bdb01f574cff0cbe8c23183c4fcd6d7dd1b4128e54287c","last_block_timestamp":1758115458},"commit_batch_info":{"batch_number":10,"new_state_commitment":"0x53680ad464b20f43921708bd3e024f365b788b9e11cf49e783607a42172136fc","number_of_layer1_txs":0,"priority_operations_hash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","dependency_roots_rolling_hash":"0x0000000000000000000000000000000000000000000000000000000000000000","l2_to_l1_logs_root_hash":"0x692f35c99f9c698852289ffecf07f6dd45770904521149d79aa85aae598fa375","l2_da_validator":"0x0000000000000000000000000000000000000000","da_commitment":"0x86b130c978627d2acb4a68c823cfc31efadf6482862566d364cc4bc15e500e2b","first_block_timestamp":1758116549,"last_block_timestamp":1758116549,"chain_id":8022833,"operator_da_input":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,201,102,180,205,111,127,203,19,178,222,176,220,147,85,249,171,106,46,88,99,189,117,148,44,88,11,167,49,72,205,72,21,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,116,25,135,1,193,217,21,41,206,115,57,17,55,153,69,34,75,25,41,48,9,20,117,70,62,143,98,164,122,16,216,160,0,0,0,2,193,25,138,114,80,95,70,215,34,237,142,12,160,249,191,228,43,163,162,216,104,166,24,217,213,90,128,186,146,85,247,97,20,33,1,64,111,64,166,72,80,155,187,230,197,73,156,145,87,2,137,219,217,151,57,45,241,113,145,154,157,86,109,62,141,1,57,228,183,230,28,9,1,34,1,64,111,64,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"protocol_version":"0.31.0","upgrade_tx_hash":null},"chain_address":"0x02b1ac1cf0a592aefd3c2246b2431388365db272","blob_sidecar":null,"first_block_number":10,"last_block_number":10,"tx_count":1,"execution_version":1},"data":{"Real":[2,252,54,244]}}"#;
        let b = serde_json::from_str::<SignedBatchEnvelope<FriProof>>(data).unwrap();
        assert!(matches!(b.data, FriProof::Real(RealFriProof::V1(_))));
    }
}
