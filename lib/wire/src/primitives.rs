//! Primitive network types that are not expected to change over time.

use alloy::primitives::bytes::BufMut;
use alloy::primitives::{B256, Bytes, U256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};

#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct ForcedPreimage {
    pub hash: B256,
    pub preimage: Bytes,
}

/// Represents 256 consecutive block hashes. It should not be necessary to transport all of them over
/// network but this is kept for now as a short-cut.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockHashes(pub [U256; 256]);

impl Encodable for BlockHashes {
    fn encode(&self, out: &mut dyn BufMut) {
        alloy::rlp::encode_list(&self.0, out);
    }

    fn length(&self) -> usize {
        alloy::rlp::list_length(&self.0)
    }
}

impl Decodable for BlockHashes {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let vec: Vec<U256> = Vec::decode(buf)?;
        let array: [U256; 256] = vec
            .try_into()
            .map_err(|_| alloy::rlp::Error::Custom("expected array of length 256"))?;
        Ok(Self(array))
    }
}
