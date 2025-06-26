use zksync_consensus_engine::Last;
use zksync_consensus_roles::validator;
use crate::model::ReplayRecord;

/// Block certificate.
#[derive(Debug, PartialEq, Clone)]
pub enum BlockCertificate {
    V1(validator::v1::CommitQC),
    V2(validator::v2::CommitQC),
}

impl BlockCertificate {
    /// Returns block number.
    pub fn number(&self) -> validator::BlockNumber {
        match self {
            Self::V1(qc) => qc.message.proposal.number,
            Self::V2(qc) => qc.message.proposal.number,
        }
    }

    /// Returns payload hash.
    pub fn payload_hash(&self) -> validator::PayloadHash {
        match self {
            Self::V1(qc) => qc.message.proposal.payload,
            Self::V2(qc) => qc.message.proposal.payload,
        }
    }
}

impl From<BlockCertificate> for Last {
    fn from(cert: BlockCertificate) -> Self {
        match cert {
            BlockCertificate::V1(qc) => Last::FinalV1(qc),
            BlockCertificate::V2(qc) => Last::FinalV2(qc),
        }
    }
}

/// Block metadata.
#[derive(Debug, PartialEq, Clone)]
pub struct BlockMetadata {
    pub payload_hash: validator::PayloadHash,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Payload(pub ReplayRecord);

impl Payload {
    pub fn decode(payload: &validator::Payload) -> anyhow::Result<Self> {
        zksync_protobuf::decode(&payload.0)
    }

    pub fn encode(&self) -> validator::Payload {
        validator::Payload(zksync_protobuf::encode(self))
    }
}

#[derive(Clone, Debug)]
pub enum ReducedBlockCommand {
    Replay(ReplayRecord),
    Produce(u64),
}
