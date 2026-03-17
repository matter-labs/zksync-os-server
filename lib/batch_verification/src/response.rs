use serde::{Deserialize, Serialize};
use tokio_util::codec::{self, LengthDelimitedCodec};

use zksync_os_batch_types::BatchSignature;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum BatchVerificationResult {
    Success(BatchSignature),
    Refused(String),
}

/// Response sent from external nodes back to main sequencer
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BatchVerificationResponse {
    pub request_id: u64,
    pub batch_number: u64,
    pub result: BatchVerificationResult,
}

pub struct BatchVerificationResponseDecoder {
    inner: LengthDelimitedCodec,
}

impl Default for BatchVerificationResponseDecoder {
    fn default() -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
        }
    }
}

impl BatchVerificationResponseDecoder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl codec::Decoder for BatchVerificationResponseDecoder {
    type Item = BatchVerificationResponse;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.inner
            .decode(src)?
            .map(|bytes| {
                BatchVerificationResponse::decode(&bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })
            .transpose()
    }
}

pub struct BatchVerificationResponseCodec {
    inner: LengthDelimitedCodec,
}

impl Default for BatchVerificationResponseCodec {
    fn default() -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
        }
    }
}

impl BatchVerificationResponseCodec {
    pub fn new() -> Self {
        Self::default()
    }
}

impl codec::Encoder<BatchVerificationResponse> for BatchVerificationResponseCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: BatchVerificationResponse,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.inner.encode(item.encode().into(), dst)
    }
}
