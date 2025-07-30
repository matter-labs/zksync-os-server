use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// A log produced by a transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2ToL1Log {
    /// The L2 address which sent the log.
    /// For user messages set to `L1Messenger` system hook address,
    /// for l1 -> l2 txs logs - `BootloaderFormalAddress`.
    pub sender: Address,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message sender address(padded),
    /// for l1 -> l2 txs logs - transaction hash.
    pub key: B256,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message hash.
    /// for l1 -> l2 txs logs - success flag(padded).
    pub value: B256,
}

impl alloy::rlp::Encodable for L2ToL1Log {
    fn encode(&self, out: &mut dyn alloy::rlp::BufMut) {
        let payload_length = self.sender.length() + self.key.length() + self.value.length();

        alloy::rlp::Header {
            list: true,
            payload_length,
        }
        .encode(out);
        self.sender.encode(out);
        self.key.encode(out);
        self.value.encode(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.sender.length() + self.key.length() + self.value.length();
        payload_length + alloy::rlp::length_of_length(payload_length)
    }
}

impl alloy::rlp::Decodable for L2ToL1Log {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy::rlp::Error> {
        let h = alloy::rlp::Header::decode(buf)?;
        let pre = buf.len();

        let sender = alloy::rlp::Decodable::decode(buf)?;
        let key = alloy::rlp::Decodable::decode(buf)?;
        let value = alloy::rlp::Decodable::decode(buf)?;

        if h.payload_length != pre - buf.len() {
            return Err(alloy::rlp::Error::Custom("did not consume exact payload"));
        }

        Ok(Self { sender, key, value })
    }
}
