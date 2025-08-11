mod encode;
mod l1;
mod l2;

pub use encode::*;
pub use l1::*;
pub use l2::*;
use std::fmt;

use alloy::consensus::crypto::RecoveryError;
use alloy::consensus::transaction::{Recovered, SignerRecoverable};
use alloy::consensus::{Transaction, TransactionEnvelope};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, TxNonce};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

// `TransactionEnvelope` derive macro below depends on this being present
use alloy::rlp as alloy_rlp;

/// ZKsync OS transaction envelope describing both [EIP-2718] envelopes and custom L1->L2 transaction
/// envelope.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Clone, Debug, TransactionEnvelope)]
#[envelope(alloy_consensus = alloy::consensus, tx_type_name = ZkTxType)]
pub enum ZkEnvelope {
    #[envelope(ty = 42)]
    L1(L1Envelope),
    #[envelope(flatten)]
    L2(L2Envelope),
}

impl ZkEnvelope {
    /// Returns the [`ZkTxType`] of the inner transaction.
    pub const fn tx_type(&self) -> ZkTxType {
        match self {
            Self::L1(_) => ZkTxType::L1,
            Self::L2(l2_tx) => ZkTxType::L2(l2_tx.tx_type()),
        }
    }

    /// Recovers the signer of inner transaction and returns a `ZkTransaction`.
    pub fn try_into_recovered(self) -> Result<ZkTransaction, RecoveryError> {
        match self {
            Self::L1(l1_tx) => Ok(ZkTransaction::from(l1_tx)),
            Self::L2(l2_tx) => Ok(ZkTransaction::from(SignerRecoverable::try_into_recovered(
                l2_tx,
            )?)),
        }
    }
}

/// ZKsync OS transaction with a known signer (usually EC recovered or simulated). Unlike alloy/reth
/// we mostly operate on this type as ZKsync OS expects signer to be provided externally (e.g., from
/// the sequencer). This could change in the future.
#[derive(Clone, Debug)]
pub struct ZkTransaction {
    pub inner: Recovered<ZkEnvelope>,
}

impl Serialize for ZkTransaction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner.encoded_2718().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ZkTransaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            ZkEnvelope::decode_2718(&mut Vec::<u8>::deserialize(deserializer)?.as_slice())
                .expect("Failed to decode 2718 transaction")
                .try_into_recovered()
                .expect("Failed to recover transaction's signer"),
        )
    }
}

impl ZkTransaction {
    pub fn envelope(&self) -> &ZkEnvelope {
        self.inner.inner()
    }

    pub fn into_envelope(self) -> ZkEnvelope {
        self.inner.into_inner()
    }

    pub fn hash(&self) -> &B256 {
        match self.envelope() {
            ZkEnvelope::L1(l1_tx) => l1_tx.hash(),
            ZkEnvelope::L2(l2_tx) => l2_tx.hash(),
        }
    }

    pub fn signer(&self) -> Address {
        self.inner.signer()
    }

    pub fn nonce(&self) -> TxNonce {
        self.inner.nonce()
    }

    pub fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    pub const fn tx_type(&self) -> ZkTxType {
        self.inner.inner().tx_type()
    }

    pub fn into_parts(self) -> (ZkEnvelope, Address) {
        self.inner.into_parts()
    }
}

impl From<L1Envelope> for ZkTransaction {
    fn from(value: L1Envelope) -> Self {
        let signer = value.inner.tx().from;
        Self {
            inner: Recovered::new_unchecked(ZkEnvelope::L1(value), signer),
        }
    }
}

impl From<L2Transaction> for ZkTransaction {
    fn from(value: L2Transaction) -> Self {
        let (tx, signer) = value.into_parts();
        Self {
            inner: Recovered::new_unchecked(ZkEnvelope::L2(tx), signer),
        }
    }
}

impl fmt::Display for ZkTxType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::L2(tx) => tx.fmt(f),
            Self::L1 => write!(f, "L1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::TxType;
    use alloy::consensus::private::alloy_primitives;
    use alloy::primitives::{TxKind, address};

    #[test]
    // Test vector from https://etherscan.io/tx/0x280cde7cdefe4b188750e76c888f13bd05ce9a4d7767730feefe8a0e50ca6fc4
    fn test_decode_live_legacy_tx() {
        use alloy_primitives::address;

        let raw_tx = alloy_primitives::bytes!(
            "f9015482078b8505d21dba0083022ef1947a250d5630b4cf539739df2c5dacb4c659f2488d880c46549a521b13d8b8e47ff36ab50000000000000000000000000000000000000000000066ab5a608bd00a23f2fe000000000000000000000000000000000000000000000000000000000000008000000000000000000000000048c04ed5691981c42154c6167398f95e8f38a7ff00000000000000000000000000000000000000000000000000000000632ceac70000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000006c6ee5e31d828de241282b9606c8e98ea48526e225a0c9077369501641a92ef7399ff81c21639ed4fd8fc69cb793cfa1dbfab342e10aa0615facb2f1bcf3274a354cfe384a38d0cc008a11c2dd23a69111bc6930ba27a8"
        );
        let res = ZkEnvelope::fallback_decode(&mut raw_tx.as_ref()).unwrap();
        assert_eq!(res.tx_type(), ZkTxType::L2(TxType::Legacy));

        let tx = match res {
            ZkEnvelope::L2(L2Envelope::Legacy(tx)) => tx,
            _ => unreachable!(),
        };

        assert_eq!(tx.tx().chain_id(), Some(1));

        assert_eq!(
            tx.tx().to,
            TxKind::Call(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"))
        );
        assert_eq!(
            tx.hash().to_string(),
            "0x280cde7cdefe4b188750e76c888f13bd05ce9a4d7767730feefe8a0e50ca6fc4"
        );
        let from = tx.recover_signer().unwrap();
        assert_eq!(from, address!("a12e1462d0ceD572f396F58B6E2D03894cD7C8a4"));
    }
}
