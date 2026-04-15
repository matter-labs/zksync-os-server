use alloy::primitives::Bytes;
use alloy::primitives::bytes::BufMut;
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};

/// Batch verification request sent by the main node to authenticated verifier peers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct VerifyBatch {
    pub request_id: u64,
    pub batch_number: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub pubdata_mode: u8,
    pub commit_data: Bytes,
    pub prev_commit_data: Bytes,
    pub execution_protocol_version: u16,
}

/// Batch verification response sent by a verifier peer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VerifyBatchResult {
    pub request_id: u64,
    pub batch_number: u64,
    pub result: VerifyBatchOutcome,
}

/// Result of verifier-peer processing for a [`VerifyBatch`] request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VerifyBatchOutcome {
    /// The verifier peer approved the batch and returned its signature bytes.
    Approved(Bytes),
    /// The verifier peer refused the batch and returned a reason string.
    Refused(String),
}

impl Encodable for VerifyBatchResult {
    fn encode(&self, out: &mut dyn BufMut) {
        self.request_id.encode(out);
        self.batch_number.encode(out);
        match &self.result {
            VerifyBatchOutcome::Approved(signature) => {
                0u8.encode(out);
                signature.encode(out);
            }
            VerifyBatchOutcome::Refused(reason) => {
                1u8.encode(out);
                reason.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        self.request_id.length()
            + self.batch_number.length()
            + 1u8.length()
            + match &self.result {
                VerifyBatchOutcome::Approved(signature) => signature.length(),
                VerifyBatchOutcome::Refused(reason) => reason.length(),
            }
    }
}

impl Decodable for VerifyBatchResult {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let request_id = u64::decode(buf)?;
        let batch_number = u64::decode(buf)?;
        let tag = u8::decode(buf)?;
        let result = match tag {
            0 => VerifyBatchOutcome::Approved(Bytes::decode(buf)?),
            1 => VerifyBatchOutcome::Refused(String::decode(buf)?),
            _ => return Err(alloy_rlp::Error::Custom("invalid verify batch result tag")),
        };
        Ok(Self {
            request_id,
            batch_number,
            result,
        })
    }
}
