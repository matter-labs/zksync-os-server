use crate::receipt::ZkReceipt;
use crate::transaction::{L1PriorityTxType, L1TxType, TxType};
use crate::{L2ToL1Log, UpgradeTxType, ZkTxType};
use alloy::consensus::{Eip658Value, ReceiptWithBloom, TxReceipt};
use alloy::eips::Typed2718;
use alloy::eips::eip2718::{
    Decodable2718, EIP1559_TX_TYPE_ID, EIP2930_TX_TYPE_ID, EIP4844_TX_TYPE_ID, EIP7702_TX_TYPE_ID,
    Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718, LEGACY_TX_TYPE_ID,
};
use alloy::primitives::{Bloom, Log};
use alloy::rlp::{BufMut, Decodable, Encodable};
use core::fmt;
use serde::{Deserialize, Serialize};

/// Receipt envelope, as defined in [EIP-2718], that also includes custom ZKsync OS receipt types.
///
/// This enum distinguishes between tagged and untagged legacy receipts, as the
/// in-protocol Merkle tree may commit to EITHER 0-prefixed or raw. Therefore
/// we must ensure that encoding returns the precise byte-array that was
/// decoded, preserving the presence or absence of the `TransactionType` flag.
///
/// Transaction receipt payloads are specified in their respective EIPs.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ZkReceiptEnvelope<T = Log, U = L2ToL1Log> {
    /// Receipt envelope with no type flag.
    #[serde(rename = "0x0", alias = "0x00")]
    Legacy(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 1, containing a [EIP-2930] receipt.
    ///
    /// [EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930
    #[serde(rename = "0x1", alias = "0x01")]
    Eip2930(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 2, containing a [EIP-1559] receipt.
    ///
    /// [EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559
    #[serde(rename = "0x2", alias = "0x02")]
    Eip1559(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 3, containing a [EIP-4844] receipt.
    ///
    /// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
    #[serde(rename = "0x3", alias = "0x03")]
    Eip4844(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 4, containing a [EIP-7702] receipt.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    #[serde(rename = "0x4", alias = "0x04")]
    Eip7702(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 255, containing an L1->L2 priority transaction receipt.
    #[serde(rename = "0x7f")]
    L1(ReceiptWithBloom<ZkReceipt<T, U>>),
    /// Receipt envelope with type flag 254, containing an upgrade transaction receipt.
    #[serde(rename = "0x7e")]
    Upgrade(ReceiptWithBloom<ZkReceipt<T, U>>),
}

impl<T, U> ZkReceiptEnvelope<T, U> {
    /// Creates the envelope for a given type and receipt.
    pub fn from_typed<R>(tx_type: ZkTxType, receipt: R) -> Self
    where
        R: Into<ReceiptWithBloom<ZkReceipt<T, U>>>,
    {
        match tx_type {
            ZkTxType::L2(TxType::Legacy) => Self::Legacy(receipt.into()),
            ZkTxType::L2(TxType::Eip2930) => Self::Eip2930(receipt.into()),
            ZkTxType::L2(TxType::Eip1559) => Self::Eip1559(receipt.into()),
            ZkTxType::L2(TxType::Eip4844) => Self::Eip4844(receipt.into()),
            ZkTxType::L2(TxType::Eip7702) => Self::Eip7702(receipt.into()),
            ZkTxType::L1 => Self::L1(receipt.into()),
            ZkTxType::Upgrade => Self::Upgrade(receipt.into()),
        }
    }

    /// Converts the receipt's log type by applying a function to each log.
    ///
    /// Returns the receipt with the new log type.
    pub fn map_logs<X, Y>(
        self,
        logs_f: impl FnMut(T) -> X,
        l2_to_l1_logs_f: impl FnMut(U) -> Y,
    ) -> ZkReceiptEnvelope<X, Y> {
        match self {
            Self::Legacy(r) => {
                ZkReceiptEnvelope::Legacy(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::Eip2930(r) => {
                ZkReceiptEnvelope::Eip2930(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::Eip1559(r) => {
                ZkReceiptEnvelope::Eip1559(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::Eip4844(r) => {
                ZkReceiptEnvelope::Eip4844(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::Eip7702(r) => {
                ZkReceiptEnvelope::Eip7702(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::L1(r) => {
                ZkReceiptEnvelope::L1(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
            Self::Upgrade(r) => {
                ZkReceiptEnvelope::Upgrade(r.map_receipt(|r| r.map_logs(logs_f, l2_to_l1_logs_f)))
            }
        }
    }

    /// Return the [`TxType`] of the inner receipt.
    pub const fn tx_type(&self) -> ZkTxType {
        match self {
            Self::Legacy(_) => ZkTxType::L2(TxType::Legacy),
            Self::Eip2930(_) => ZkTxType::L2(TxType::Eip2930),
            Self::Eip1559(_) => ZkTxType::L2(TxType::Eip1559),
            Self::Eip4844(_) => ZkTxType::L2(TxType::Eip4844),
            Self::Eip7702(_) => ZkTxType::L2(TxType::Eip7702),
            Self::L1(_) => ZkTxType::L1,
            Self::Upgrade(_) => ZkTxType::Upgrade,
        }
    }

    /// Return true if the transaction was successful.
    pub const fn is_success(&self) -> bool {
        self.status()
    }

    /// Returns the success status of the receipt's transaction.
    pub const fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    /// Returns the cumulative gas used at this receipt.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    /// Return the receipt logs.
    pub fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }

    /// Consumes the type and returns the logs.
    pub fn into_logs(self) -> Vec<T> {
        self.into_receipt().logs
    }

    /// Return the receipt's bloom.
    pub const fn logs_bloom(&self) -> &Bloom {
        &self.as_receipt_with_bloom().unwrap().logs_bloom
    }

    /// Return the receipt L2->L1 logs.
    pub fn l2_to_l1_logs(&self) -> &[U] {
        &self.as_receipt().unwrap().l2_to_l1_logs
    }

    /// Consumes the type and returns the L2->L1 logs.
    pub fn into_l2_to_l1_logs(self) -> Vec<U> {
        self.into_receipt().l2_to_l1_logs
    }

    /// Return the inner receipt with bloom. Currently this is infallible,
    /// however, future receipt types may be added.
    pub const fn as_receipt_with_bloom(&self) -> Option<&ReceiptWithBloom<ZkReceipt<T, U>>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Eip7702(t)
            | Self::L1(t)
            | Self::Upgrade(t) => Some(t),
        }
    }

    /// Return the mutable inner receipt with bloom. Currently this is
    /// infallible, however, future receipt types may be added.
    pub const fn as_receipt_with_bloom_mut(
        &mut self,
    ) -> Option<&mut ReceiptWithBloom<ZkReceipt<T, U>>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Eip7702(t)
            | Self::L1(t)
            | Self::Upgrade(t) => Some(t),
        }
    }

    /// Consumes the type and returns the underlying [`Receipt`].
    pub fn into_receipt(self) -> ZkReceipt<T, U> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Eip7702(t)
            | Self::L1(t)
            | Self::Upgrade(t) => t.receipt,
        }
    }

    /// Return the inner receipt. Currently this is infallible, however, future
    /// receipt types may be added.
    pub const fn as_receipt(&self) -> Option<&ZkReceipt<T, U>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Eip7702(t)
            | Self::L1(t)
            | Self::Upgrade(t) => Some(&t.receipt),
        }
    }
}

impl<T> TxReceipt for ZkReceiptEnvelope<T>
where
    T: Clone + fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.as_receipt().unwrap().status
    }

    fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    /// Return the receipt's bloom.
    fn bloom(&self) -> Bloom {
        self.as_receipt_with_bloom().unwrap().logs_bloom
    }

    fn bloom_cheap(&self) -> Option<Bloom> {
        Some(self.bloom())
    }

    /// Returns the cumulative gas used at this receipt.
    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    /// Return the receipt logs.
    fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }

    fn into_logs(self) -> Vec<Self::Log>
    where
        Self::Log: Clone,
    {
        self.into_receipt().logs
    }
}

impl ZkReceiptEnvelope {
    /// Get the length of the inner receipt in the 2718 encoding.
    pub fn inner_length(&self) -> usize {
        self.as_receipt_with_bloom().unwrap().length()
    }

    /// Calculate the length of the rlp payload of the network encoded receipt.
    pub fn rlp_payload_length(&self) -> usize {
        let length = self.as_receipt_with_bloom().unwrap().length();
        match self {
            Self::Legacy(_) => length,
            _ => length + 1,
        }
    }
}

impl Encodable for ZkReceiptEnvelope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        self.network_len()
    }
}

impl Decodable for ZkReceiptEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Self::network_decode(buf)
            .map_or_else(|_| Err(alloy::rlp::Error::Custom("Unexpected type")), Ok)
    }
}

impl Typed2718 for ZkReceiptEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Legacy(_) => LEGACY_TX_TYPE_ID,
            Self::Eip2930(_) => EIP2930_TX_TYPE_ID,
            Self::Eip1559(_) => EIP1559_TX_TYPE_ID,
            Self::Eip4844(_) => EIP4844_TX_TYPE_ID,
            Self::Eip7702(_) => EIP7702_TX_TYPE_ID,
            Self::L1(_) => L1PriorityTxType::TX_TYPE,
            Self::Upgrade(_) => UpgradeTxType::TX_TYPE,
        }
    }
}

impl IsTyped2718 for ZkReceiptEnvelope {
    fn is_type(type_id: u8) -> bool {
        <TxType as IsTyped2718>::is_type(type_id)
    }
}

impl Encodable2718 for ZkReceiptEnvelope {
    fn encode_2718_len(&self) -> usize {
        self.inner_length() + !self.is_legacy() as usize
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        match self.type_flag() {
            None => {}
            Some(ty) => out.put_u8(ty),
        }
        self.as_receipt_with_bloom().unwrap().encode(out);
    }
}

impl Decodable2718 for ZkReceiptEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        let receipt = Decodable::decode(buf)?;
        match ty
            .try_into()
            .map_err(|_| alloy::rlp::Error::Custom("Unexpected type"))?
        {
            ZkTxType::L2(TxType::Eip2930) => Ok(Self::Eip2930(receipt)),
            ZkTxType::L2(TxType::Eip1559) => Ok(Self::Eip1559(receipt)),
            ZkTxType::L2(TxType::Eip4844) => Ok(Self::Eip4844(receipt)),
            ZkTxType::L2(TxType::Eip7702) => Ok(Self::Eip7702(receipt)),
            ZkTxType::L2(TxType::Legacy) => Err(Eip2718Error::UnexpectedType(0)),
            ZkTxType::L1 => Ok(Self::L1(receipt)),
            ZkTxType::Upgrade => Ok(Self::Upgrade(receipt)),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(Decodable::decode(buf)?))
    }
}
