mod encode;
mod l1;
mod l2;

pub use encode::*;
pub use l1::*;
pub use l2::*;

use alloy::consensus::crypto::RecoveryError;
use alloy::consensus::transaction::{Recovered, SignerRecoverable};
use alloy::consensus::{Transaction, TransactionEnvelope};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256};
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkTransaction {
    #[serde(flatten)]
    pub inner: Recovered<ZkEnvelope>,
}

impl ZkTransaction {
    pub fn envelope(&self) -> &ZkEnvelope {
        self.inner.inner()
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
