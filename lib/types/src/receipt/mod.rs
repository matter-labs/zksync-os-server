mod envelope;
pub use envelope::ZkReceiptEnvelope;

use crate::log::L2ToL1Log;
use alloy::consensus::private::alloy_rlp;
use alloy::consensus::{
    Eip658Value, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt,
};
use alloy::primitives::{Bloom, Log};
use alloy::rlp::{BufMut, Decodable, Encodable, Header};
use core::fmt;
use serde::{Deserialize, Serialize};

/// Receipt containing result of transaction execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[doc(alias = "TransactionReceipt", alias = "TxReceipt")]
pub struct ZkReceipt<T = Log, U = L2ToL1Log> {
    /// If transaction is executed successfully.
    ///
    /// This is the `statusCode`
    #[serde(flatten)]
    pub status: Eip658Value,
    /// Gas used
    #[serde(with = "alloy::serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Log send from contracts.
    pub logs: Vec<T>,
    /// L2 to L1 logs generated within this transaction.
    pub l2_to_l1_logs: Vec<U>,
}

impl<T, U> ZkReceipt<T, U> {
    /// Converts the receipt's log type by applying a function to each log.
    ///
    /// Returns the receipt with the new log type
    pub fn map_logs<X, Y>(
        self,
        logs_f: impl FnMut(T) -> X,
        l2_to_l1_logs_f: impl FnMut(U) -> Y,
    ) -> ZkReceipt<X, Y> {
        let Self {
            status,
            cumulative_gas_used,
            logs,
            l2_to_l1_logs,
        } = self;
        ZkReceipt {
            status,
            cumulative_gas_used,
            logs: logs.into_iter().map(logs_f).collect(),
            l2_to_l1_logs: l2_to_l1_logs.into_iter().map(l2_to_l1_logs_f).collect(),
        }
    }
}

impl<T, U> ZkReceipt<T, U>
where
    T: AsRef<Log>,
{
    /// Calculates [`Log`]'s bloom filter. This is slow operation and
    /// [`ReceiptWithBloom`] can be used to cache this value.
    pub fn bloom_slow(&self) -> Bloom {
        self.logs.iter().map(AsRef::as_ref).collect()
    }

    /// Calculates the bloom filter for the receipt and returns the
    /// [`ReceiptWithBloom`] container type.
    pub fn with_bloom(self) -> ReceiptWithBloom<Self> {
        ReceiptWithBloom {
            logs_bloom: self.bloom_slow(),
            receipt: self,
        }
    }
}

impl<T, U> TxReceipt for ZkReceipt<T, U>
where
    T: AsRef<Log> + Clone + fmt::Debug + PartialEq + Eq + Send + Sync,
    U: Clone + fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.status
    }

    fn status(&self) -> bool {
        self.status.coerce_status()
    }

    fn bloom(&self) -> Bloom {
        self.bloom_slow()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }

    fn logs(&self) -> &[Self::Log] {
        &self.logs
    }

    fn into_logs(self) -> Vec<Self::Log>
    where
        Self::Log: Clone,
    {
        self.logs
    }
}

impl<T: Encodable, U: Encodable> ZkReceipt<T, U> {
    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.status.length()
            + self.cumulative_gas_used.length()
            + bloom.length()
            + self.logs.length()
            + self.l2_to_l1_logs.length()
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.status.encode(out);
        self.cumulative_gas_used.encode(out);
        bloom.encode(out);
        self.logs.encode(out);
        self.l2_to_l1_logs.encode(out);
    }

    /// Returns RLP header for this receipt encoding with the given [`Bloom`].
    pub fn rlp_header_with_bloom(&self, bloom: &Bloom) -> Header {
        Header {
            list: true,
            payload_length: self.rlp_encoded_fields_length_with_bloom(bloom),
        }
    }
}

impl<T: Encodable, U: Encodable> RlpEncodableReceipt for ZkReceipt<T, U> {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.rlp_header_with_bloom(bloom).length_with_payload()
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.rlp_header_with_bloom(bloom).encode(out);
        self.rlp_encode_fields_with_bloom(bloom, out);
    }
}

impl<T: Decodable, U: Decodable> ZkReceipt<T, U> {
    /// RLP-decodes receipt's field with a [`Bloom`].
    ///
    /// Does not expect an RLP header.
    pub fn rlp_decode_fields_with_bloom(
        buf: &mut &[u8],
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs_bloom = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;
        let l2_to_l1_logs = Decodable::decode(buf)?;

        Ok(ReceiptWithBloom {
            receipt: Self {
                status,
                cumulative_gas_used,
                logs,
                l2_to_l1_logs,
            },
            logs_bloom,
        })
    }
}

impl<T: Decodable, U: Decodable> RlpDecodableReceipt for ZkReceipt<T, U> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();

        let this = Self::rlp_decode_fields_with_bloom(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
    }
}

impl<T, U> From<ReceiptWithBloom<Self>> for ZkReceipt<T, U> {
    /// Consume the structure, returning only the receipt
    fn from(receipt_with_bloom: ReceiptWithBloom<Self>) -> Self {
        receipt_with_bloom.receipt
    }
}
