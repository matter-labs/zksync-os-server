use serde::{Deserialize, Serialize};
use tokio_util::codec::{self, LengthDelimitedCodec};

/// Request sent from main sequencer to external nodes for batch verification
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchVerificationRequest {
    pub batch_number: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub request_id: u64,
}

pub struct BatchVerificationRequestDecoder {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BatchVerificationRequestDecoder {
    pub fn new(wire_format_version: u32) -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version,
        }
    }
}

impl codec::Decoder for BatchVerificationRequestDecoder {
    type Item = BatchVerificationRequest;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.inner.decode(src).map(|inner| {
            inner.map(|bytes| BatchVerificationRequest::decode(&bytes, self.wire_format_version))
        })
    }
}

pub struct BatchVerificationRequestCodec(LengthDelimitedCodec);

impl BatchVerificationRequestCodec {
    pub fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Encoder<BatchVerificationRequest> for BatchVerificationRequestCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: BatchVerificationRequest,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.0
            .encode(item.encode_with_current_version().into(), dst)
    }
}
