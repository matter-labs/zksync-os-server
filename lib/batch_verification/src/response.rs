use serde::{Deserialize, Serialize};
use tokio_util::codec::{self, LengthDelimitedCodec};

use crate::BATCH_VERIFICATION_WIRE_FORMAT_VERSION;
use zksync_os_types::BatchSignature;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BatchVerificationResult {
    Success(BatchSignature),
    Refused(String),
}

/// Response sent from external nodes back to main sequencer
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchVerificationResponse {
    pub request_id: u64,
    pub result: BatchVerificationResult,
}

pub struct BatchVerificationResponseDecoder {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BatchVerificationResponseDecoder {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version: BATCH_VERIFICATION_WIRE_FORMAT_VERSION, // server always uses the latest version
        }
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
                BatchVerificationResponse::decode(&bytes, self.wire_format_version)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })
            .transpose()
    }
}

pub struct BatchVerificationResponseCodec {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BatchVerificationResponseCodec {
    pub fn new(wire_format_version: u32) -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version,
        }
    }
}

impl codec::Encoder<BatchVerificationResponse> for BatchVerificationResponseCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: BatchVerificationResponse,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.inner.encode(
            item.encode_with_version(self.wire_format_version).into(),
            dst,
        )
    }
}
