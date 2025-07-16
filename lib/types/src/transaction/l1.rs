use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::consensus::{Signed, Transaction, Typed2718};
use alloy::eips::eip2718::{Eip2718Error, Eip2718Result};
use alloy::eips::eip2930::AccessList;
use alloy::eips::eip7702::SignedAuthorization;
use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::{
    keccak256, Address, Bytes, ChainId, Signature, TxHash, TxKind, B256, U256,
};
use alloy::rlp::{BufMut, Decodable, Encodable};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use std::hash::Hash;
use zksync_os_contract_interface::L2CanonicalTransaction;

// L1 transactions are not encodable when we use type id 255 so we pretend like they have type 42
// for all external means. VM and L1->L2 communication still uses 255.
pub const FAKE_L1_PRIORITY_TX_TYPE_ID: u8 = 42;
pub const REAL_L1_PRIORITY_TX_TYPE_ID: u8 = 255;

/// An L1->L2 priority transaction.
///
/// Specific to ZKsync OS and hence has a custom transaction type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxL1Priority {
    pub hash: TxHash,
    /// The 160-bit address of the initiator on L1.
    pub from: Address,
    /// The 160-bit address of the message call’s recipient. Cannot be missing as L1->L2 priority
    /// transaction cannot be `Create`.
    pub to: Address,
    /// A scalar value equal to the maximum amount of L2 gas that should be used in executing this
    /// transaction on L2. This is paid up-front before any computation is done and may not be
    /// increased later.
    #[serde(with = "alloy::serde::quantity")]
    pub gas_limit: u64,
    /// Maximum amount of L2 gas that will cost to publish one byte of pubdata (every piece of data
    /// that will be stored on L1).
    #[serde(with = "alloy::serde::quantity")]
    pub gas_per_pubdata_byte_limit: u64,
    /// The absolute maximum sender is willing to pay per unit of L2 gas to get the transaction
    /// included in a block. Analog to the EIP-1559 `maxFeePerGas` for L1->L2 priority transactions.
    #[serde(with = "alloy::serde::quantity")]
    pub max_fee_per_gas: u128,
    /// The additional fee that is paid directly to the validator to incentivize them to include the
    /// transaction in a block. Analog to the EIP-1559 `maxPriorityFeePerGas` for L1->L2 priority
    /// transactions.
    #[serde(with = "alloy::serde::quantity")]
    pub max_priority_fee_per_gas: u128,
    /// Priority operation id that is sequential for the entire chain. Presented as nonce of the
    /// transaction.
    #[serde(with = "alloy::serde::quantity")]
    pub nonce: u64,
    /// A scalar value equal to the number of Wei to be transferred to the message call’s recipient.
    pub value: U256,
    /// The amount of base token that should be minted on L2 as the result of this transaction.
    pub to_mint: U256,
    /// The recipient of the refund for the transaction on L2. If the transaction fails, then this
    /// address will receive the `value` of this transaction.
    pub refund_recipient: Address,
    /// data: An unlimited size byte array specifying the input data of the message call.
    pub input: Bytes,
    /// The set of L2 bytecode hashes whose preimages were shown on L1.
    pub factory_deps: Vec<B256>,
}

impl Typed2718 for TxL1Priority {
    fn ty(&self) -> u8 {
        FAKE_L1_PRIORITY_TX_TYPE_ID
    }
}

impl RlpEcdsaEncodableTx for TxL1Priority {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.hash.length()
            + self.from.length()
            + self.to.length()
            + self.gas_limit.length()
            + self.gas_per_pubdata_byte_limit.length()
            + self.max_fee_per_gas.length()
            + self.max_priority_fee_per_gas.length()
            + self.nonce.length()
            + self.value.length()
            + self.to_mint.length()
            + self.refund_recipient.length()
            + self.input.length()
            + self.factory_deps.length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.hash.encode(out);
        self.from.encode(out);
        self.to.encode(out);
        self.gas_limit.encode(out);
        self.gas_per_pubdata_byte_limit.encode(out);
        self.max_fee_per_gas.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.nonce.encode(out);
        self.value.encode(out);
        self.to_mint.encode(out);
        self.refund_recipient.encode(out);
        self.input.encode(out);
        self.factory_deps.encode(out);
    }

    fn tx_hash_with_type(&self, _signature: &Signature, _ty: u8) -> TxHash {
        self.hash
    }
}

impl RlpEcdsaDecodableTx for TxL1Priority {
    const DEFAULT_TX_TYPE: u8 = FAKE_L1_PRIORITY_TX_TYPE_ID;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            hash: Decodable::decode(buf)?,
            from: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            gas_per_pubdata_byte_limit: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            to_mint: Decodable::decode(buf)?,
            refund_recipient: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
            factory_deps: Decodable::decode(buf)?,
        })
    }
}

impl Encodable for TxL1Priority {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxL1Priority {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

impl Transaction for TxL1Priority {
    fn chain_id(&self) -> Option<ChainId> {
        None
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    fn gas_price(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.max_fee_per_gas, |base_fee| {
            // if the tip is greater than the max priority fee per gas, set it to the max
            // priority fee per gas + base fee
            let tip = self.max_fee_per_gas.saturating_sub(base_fee as u128);
            if tip > self.max_priority_fee_per_gas {
                self.max_priority_fee_per_gas + base_fee as u128
            } else {
                // otherwise return the max fee per gas
                self.max_fee_per_gas
            }
        })
    }

    fn is_dynamic_fee(&self) -> bool {
        true
    }

    fn kind(&self) -> TxKind {
        TxKind::Call(self.to)
    }

    fn is_create(&self) -> bool {
        false
    }

    fn value(&self) -> U256 {
        self.value
    }

    fn input(&self) -> &Bytes {
        &self.input
    }

    fn access_list(&self) -> Option<&AccessList> {
        None
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

/// Transaction envelope for L1->L2 priority transactions. Mostly needed as an intermediary level for
/// `ZkEnvelope`.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct L1Envelope {
    pub inner: Signed<TxL1Priority>,
}

impl L1Envelope {
    pub fn hash(&self) -> &B256 {
        self.inner.hash()
    }

    pub fn priority_id(&self) -> u64 {
        self.inner.tx().nonce
    }
}

impl Transaction for L1Envelope {
    fn chain_id(&self) -> Option<ChainId> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl Typed2718 for L1Envelope {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl Decodable2718 for L1Envelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self {
            inner: Signed::<TxL1Priority>::typed_decode(ty, buf)?,
        })
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        // Do not try to decode untyped transactions
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Encodable2718 for L1Envelope {
    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner.encode_2718(out)
    }
}

impl Decodable for L1Envelope {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let inner = Signed::<TxL1Priority>::decode_2718(buf)?;
        Ok(L1Envelope { inner })
    }
}

impl Encodable for L1Envelope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.tx().encode(out)
    }
}

impl TryFrom<L2CanonicalTransaction> for L1Envelope {
    type Error = L1EnvelopeError;

    fn try_from(tx: L2CanonicalTransaction) -> Result<Self, Self::Error> {
        let tx_type = tx.txType.saturating_to();
        if tx_type != REAL_L1_PRIORITY_TX_TYPE_ID {
            return Err(L1EnvelopeError::IncorrectTransactionType(tx_type));
        }
        if !tx.maxPriorityFeePerGas.is_zero() {
            return Err(L1EnvelopeError::NonZeroPriorityFee(tx.maxPriorityFeePerGas));
        }
        if !tx.paymaster.is_zero() {
            return Err(L1EnvelopeError::NonZeroPaymaster(tx.paymaster));
        }
        if !tx.factoryDeps.is_empty() {
            // fixme: we allow factory deps for now as current L1 setup contains a few transactions
            //        that have them by default
            // return Err(L1EnvelopeError::NonEmptyFactoryDeps(
            //     tx.factoryDeps.into_iter().map(B256::from).collect(),
            // ));
        }
        if !tx.reserved[2].is_zero() {
            return Err(L1EnvelopeError::NonZeroReservedField(2, tx.reserved[2]));
        }
        if !tx.reserved[3].is_zero() {
            return Err(L1EnvelopeError::NonZeroReservedField(3, tx.reserved[3]));
        }
        if !tx.signature.is_empty() {
            return Err(L1EnvelopeError::NonEmptySignature(tx.signature));
        }
        if !tx.paymasterInput.is_empty() {
            return Err(L1EnvelopeError::NonEmptyPaymasterInput(tx.paymasterInput));
        }
        if !tx.reservedDynamic.is_empty() {
            return Err(L1EnvelopeError::NonEmptyReservedDynamic(tx.reservedDynamic));
        }

        let hash = keccak256(tx.abi_encode());
        let tx = TxL1Priority {
            hash,
            from: Address::from_slice(&tx.from.to_be_bytes::<32>()[12..]),
            to: Address::from_slice(&tx.to.to_be_bytes::<32>()[12..]),
            gas_limit: tx.gasLimit.saturating_to(),
            gas_per_pubdata_byte_limit: tx.gasPerPubdataByteLimit.saturating_to(),
            max_fee_per_gas: tx.maxFeePerGas.saturating_to(),
            max_priority_fee_per_gas: tx.maxPriorityFeePerGas.saturating_to(),
            nonce: tx.nonce.saturating_to(),
            value: tx.value,
            to_mint: tx.reserved[0],
            refund_recipient: Address::from_slice(&tx.reserved[1].to_be_bytes::<32>()[12..]),
            input: tx.data,
            factory_deps: tx.factoryDeps.into_iter().map(B256::from).collect(),
        };
        Ok(L1Envelope {
            inner: Signed::new_unchecked(
                tx,
                // Mocked signature as L1->L2 priority transactions do not have signatures
                Signature::new(U256::ZERO, U256::ZERO, false),
                B256::from(hash),
            ),
        })
    }
}

/// Error types from decoding and validating L1->L2 priority transactions.
#[derive(Debug, thiserror::Error)]
pub enum L1EnvelopeError {
    #[error("invalid transaction type: {0}")]
    IncorrectTransactionType(u8),
    #[error("non-zero priority fee: {0}")]
    NonZeroPriorityFee(U256),
    #[error("non-zero paymaster: {0}")]
    NonZeroPaymaster(U256),
    #[error("non-empty factory deps: {0:?}")]
    NonEmptyFactoryDeps(Vec<B256>),
    #[error("non-zero reserved field #{0}: {1}")]
    NonZeroReservedField(usize, U256),
    #[error("non-empty signature: {0:?}")]
    NonEmptySignature(Bytes),
    #[error("non-empty paymaster input: {0:?}")]
    NonEmptyPaymasterInput(Bytes),
    #[error("non-empty reserved dynamic bytes: {0:?}")]
    NonEmptyReservedDynamic(Bytes),
}
