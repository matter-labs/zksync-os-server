use alloy::consensus::crypto::RecoveryError;
use alloy::consensus::private::alloy_primitives;
use alloy::consensus::transaction::{Recovered, RlpEcdsaEncodableTx, SignerRecoverable};
use alloy::consensus::{
    EthereumTypedTransaction, SignableTransaction, Signed, TransactionEnvelope, TxEip1559,
    TxEip2930, TxEip4844Variant, TxEip7702, TxLegacy,
};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::primitives::{B256, Signature};
use std::fmt;

/// L2 transaction with a known signer (usually EC recovered or simulated). Unlike alloy/reth we
/// mostly operate on this type as ZKsync OS expects signer to be provided externally (e.g., from the
/// sequencer). This could change in the future.
pub type L2Transaction = Recovered<L2Envelope>;

/// Although ZKsync OS does not support EIP-4844 transactions right now, we future-proof by using a
/// sidecar-agnostic EIP-4844 variant. Moreover, sidecar itself can have one of two forms: EIP-4844
/// or EIP-7594.
pub type L2Envelope = L2EnvelopeInner<TxEip4844Variant<BlobTransactionSidecarVariant>>;

/// `L2EnvelopeInner` re-implements the main `alloy` envelope to allow for more flexibility (extra
/// transaction types). This type describes all transactions that are executable on L2.
#[derive(Clone, Debug, TransactionEnvelope)]
#[envelope(alloy_consensus = alloy::consensus, tx_type_name = TxType)]
pub enum L2EnvelopeInner<Eip4844> {
    /// An untagged [`TxLegacy`].
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),
    /// A TxEip4844 tagged with type 3.
    /// An EIP-4844 transaction has two network representations:
    /// 1 - The transaction itself, which is a regular RLP-encoded transaction and used to retrieve
    /// historical transactions..
    ///
    /// 2 - The transaction with a sidecar, which is the form used to
    /// send transactions to the network.
    #[envelope(ty = 3)]
    Eip4844(Signed<Eip4844>),
    /// A [`TxEip7702`] tagged with type 4.
    #[envelope(ty = 4)]
    Eip7702(Signed<TxEip7702>),
}

impl<T, Eip4844> From<Signed<T>> for L2EnvelopeInner<Eip4844>
where
    EthereumTypedTransaction<Eip4844>: From<T>,
    T: RlpEcdsaEncodableTx,
{
    fn from(v: Signed<T>) -> Self {
        let (tx, sig, hash) = v.into_parts();
        let typed = EthereumTypedTransaction::from(tx);
        match typed {
            EthereumTypedTransaction::Legacy(tx_legacy) => {
                let tx = Signed::new_unchecked(tx_legacy, sig, hash);
                Self::Legacy(tx)
            }
            EthereumTypedTransaction::Eip2930(tx_eip2930) => {
                let tx = Signed::new_unchecked(tx_eip2930, sig, hash);
                Self::Eip2930(tx)
            }
            EthereumTypedTransaction::Eip1559(tx_eip1559) => {
                let tx = Signed::new_unchecked(tx_eip1559, sig, hash);
                Self::Eip1559(tx)
            }
            EthereumTypedTransaction::Eip4844(tx_eip4844_variant) => {
                let tx = Signed::new_unchecked(tx_eip4844_variant, sig, hash);
                Self::Eip4844(tx)
            }
            EthereumTypedTransaction::Eip7702(tx_eip7702) => {
                let tx = Signed::new_unchecked(tx_eip7702, sig, hash);
                Self::Eip7702(tx)
            }
        }
    }
}

impl<Eip4844> L2EnvelopeInner<Eip4844> {
    /// Return the [`TxType`] of the inner txn.
    pub const fn tx_type(&self) -> TxType {
        match self {
            Self::Legacy(_) => TxType::Legacy,
            Self::Eip2930(_) => TxType::Eip2930,
            Self::Eip1559(_) => TxType::Eip1559,
            Self::Eip4844(_) => TxType::Eip4844,
            Self::Eip7702(_) => TxType::Eip7702,
        }
    }
}

impl<Eip4844: RlpEcdsaEncodableTx> L2EnvelopeInner<Eip4844> {
    /// Returns true if the transaction is a legacy transaction.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Returns true if the transaction is an EIP-2930 transaction.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930(_))
    }

    /// Returns true if the transaction is an EIP-1559 transaction.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559(_))
    }

    /// Returns true if the transaction is an EIP-4844 transaction.
    #[inline]
    pub const fn is_eip4844(&self) -> bool {
        matches!(self, Self::Eip4844(_))
    }

    /// Returns true if the transaction is an EIP-7702 transaction.
    #[inline]
    pub const fn is_eip7702(&self) -> bool {
        matches!(self, Self::Eip7702(_))
    }

    /// Returns true if the transaction is replay protected.
    ///
    /// All non-legacy transactions are replay protected, as the chain id is
    /// included in the transaction body. Legacy transactions are considered
    /// replay protected if the `v` value is not 27 or 28, according to the
    /// rules of [EIP-155].
    ///
    /// [EIP-155]: https://eips.ethereum.org/EIPS/eip-155
    #[inline]
    pub const fn is_replay_protected(&self) -> bool {
        match self {
            Self::Legacy(tx) => tx.tx().chain_id.is_some(),
            _ => true,
        }
    }

    /// Returns the [`TxLegacy`] variant if the transaction is a legacy transaction.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip2930`] variant if the transaction is an EIP-2930 transaction.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip1559`] variant if the transaction is an EIP-1559 transaction.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip4844Variant`] variant if the transaction is an EIP-4844 transaction.
    pub const fn as_eip4844(&self) -> Option<&Signed<Eip4844>> {
        match self {
            Self::Eip4844(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip7702`] variant if the transaction is an EIP-7702 transaction.
    pub const fn as_eip7702(&self) -> Option<&Signed<TxEip7702>> {
        match self {
            Self::Eip7702(tx) => Some(tx),
            _ => None,
        }
    }

    /// Calculate the signing hash for the transaction.
    pub fn signature_hash(&self) -> B256
    where
        Eip4844: SignableTransaction<Signature>,
    {
        match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip4844(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
        }
    }

    /// Return the reference to signature.
    pub const fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip4844(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
        }
    }

    /// Return the hash of the inner Signed.
    pub fn tx_hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip4844(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
        }
    }

    /// Reference to transaction hash. Used to identify transaction.
    pub fn hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip4844(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
        }
    }

    /// Return the length of the inner txn, including type byte length
    pub fn eip2718_encoded_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.eip2718_encoded_length(),
            Self::Eip2930(t) => t.eip2718_encoded_length(),
            Self::Eip1559(t) => t.eip2718_encoded_length(),
            Self::Eip4844(t) => t.eip2718_encoded_length(),
            Self::Eip7702(t) => t.eip2718_encoded_length(),
        }
    }
}

impl<Eip4844> SignerRecoverable for L2EnvelopeInner<Eip4844>
where
    Eip4844: RlpEcdsaEncodableTx + SignableTransaction<Signature>,
{
    fn recover_signer(&self) -> Result<alloy_primitives::Address, RecoveryError> {
        match self {
            Self::Legacy(tx) => crate::transaction::SignerRecoverable::recover_signer(tx),
            Self::Eip2930(tx) => crate::transaction::SignerRecoverable::recover_signer(tx),
            Self::Eip1559(tx) => crate::transaction::SignerRecoverable::recover_signer(tx),
            Self::Eip4844(tx) => crate::transaction::SignerRecoverable::recover_signer(tx),
            Self::Eip7702(tx) => crate::transaction::SignerRecoverable::recover_signer(tx),
        }
    }

    fn recover_signer_unchecked(&self) -> Result<alloy_primitives::Address, RecoveryError> {
        match self {
            Self::Legacy(tx) => crate::transaction::SignerRecoverable::recover_signer_unchecked(tx),
            Self::Eip2930(tx) => {
                crate::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip1559(tx) => {
                crate::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip4844(tx) => {
                crate::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip7702(tx) => {
                crate::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
        }
    }

    fn recover_unchecked_with_buf(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<alloy_primitives::Address, RecoveryError> {
        match self {
            Self::Legacy(tx) => {
                crate::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip2930(tx) => {
                crate::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip1559(tx) => {
                crate::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip4844(tx) => {
                crate::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip7702(tx) => {
                crate::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
        }
    }
}

#[cfg(feature = "reth")]
impl<T: reth_primitives_traits::InMemorySize> reth_primitives_traits::InMemorySize
    for L2EnvelopeInner<T>
{
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip4844(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
        }
    }
}

#[cfg(feature = "reth")]
impl<T> reth_primitives_traits::SignedTransaction for L2EnvelopeInner<T>
where
    T: RlpEcdsaEncodableTx + SignableTransaction<Signature> + Unpin,
    Self: Clone
        + PartialEq
        + Eq
        + alloy::rlp::Decodable
        + alloy::eips::Decodable2718
        + reth_primitives_traits::MaybeSerde
        + reth_primitives_traits::InMemorySize,
{
    fn tx_hash(&self) -> &alloy::primitives::TxHash {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Eip4844(tx) => tx.hash(),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for TxType {
    fn default() -> Self {
        Self::Legacy
    }
}

impl TxType {
    /// Returns true if the transaction type is Legacy.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }

    /// Returns true if the transaction type is EIP-2930.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930)
    }

    /// Returns true if the transaction type is EIP-1559.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559)
    }

    /// Returns true if the transaction type is EIP-4844.
    #[inline]
    pub const fn is_eip4844(&self) -> bool {
        matches!(self, Self::Eip4844)
    }

    /// Returns true if the transaction type is EIP-7702.
    #[inline]
    pub const fn is_eip7702(&self) -> bool {
        matches!(self, Self::Eip7702)
    }

    /// Returns true if the transaction type has dynamic fee.
    #[inline]
    pub const fn is_dynamic_fee(&self) -> bool {
        matches!(self, Self::Eip1559 | Self::Eip4844 | Self::Eip7702)
    }
}

impl fmt::Display for TxType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy => write!(f, "Legacy"),
            Self::Eip2930 => write!(f, "EIP-2930"),
            Self::Eip1559 => write!(f, "EIP-1559"),
            Self::Eip4844 => write!(f, "EIP-4844"),
            Self::Eip7702 => write!(f, "EIP-7702"),
        }
    }
}

impl From<alloy::consensus::TxType> for TxType {
    fn from(value: alloy::consensus::TxType) -> Self {
        match value {
            alloy::consensus::TxType::Legacy => TxType::Legacy,
            alloy::consensus::TxType::Eip2930 => TxType::Eip2930,
            alloy::consensus::TxType::Eip1559 => TxType::Eip1559,
            alloy::consensus::TxType::Eip4844 => TxType::Eip4844,
            alloy::consensus::TxType::Eip7702 => TxType::Eip7702,
        }
    }
}

impl From<TxType> for alloy::consensus::TxType {
    fn from(value: TxType) -> Self {
        match value {
            TxType::Legacy => alloy::consensus::TxType::Legacy,
            TxType::Eip2930 => alloy::consensus::TxType::Eip2930,
            TxType::Eip1559 => alloy::consensus::TxType::Eip1559,
            TxType::Eip4844 => alloy::consensus::TxType::Eip4844,
            TxType::Eip7702 => alloy::consensus::TxType::Eip7702,
        }
    }
}
