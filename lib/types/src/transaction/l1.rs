use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::consensus::{Signed, Transaction, Typed2718};
use alloy::eips::eip2718::{Eip2718Error, Eip2718Result};
use alloy::eips::eip2930::AccessList;
use alloy::eips::eip7702::SignedAuthorization;
use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::{
    Address, B256, Bytes, ChainId, Signature, TxHash, TxKind, U256, keccak256,
};
use alloy::rlp::{BufMut, Decodable, Encodable};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::hash::Hash;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::L2CanonicalTransaction;

pub type L1TxSerialId = u64;
pub type L1PriorityTx = L1Tx<L1PriorityTxType>;
pub type L1PriorityEnvelope = L1Envelope<L1PriorityTxType>;

pub type L1UpgradeTx = L1Tx<UpgradeTxType>;
pub type L1UpgradeEnvelope = L1Envelope<UpgradeTxType>;

// The L1->L2 transactions are required to have the following gas per pubdata byte.
pub const REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE: u64 = 800;

// The minimal L1->L2 transaction gas limit enforced by L1 contracts to be extra safe.
pub const L1_TX_MINIMAL_GAS_LIMIT: u64 = 200_000;

pub trait L1TxType: Clone + Send + Sync + Debug + 'static {
    const TX_TYPE: u8;
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct L1PriorityTxType;

impl L1TxType for L1PriorityTxType {
    const TX_TYPE: u8 = 0x7f;
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeTxType;

impl L1TxType for UpgradeTxType {
    const TX_TYPE: u8 = 0x7e;
}

/// An L1->L2 transaction.
///
/// Specific to ZKsync OS and hence has a custom transaction type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", into = "tx_serde::TransactionSerdeHelper<T>")]
pub struct L1Tx<T: L1TxType> {
    pub hash: TxHash,
    /// The 160-bit address of the initiator on L1.
    /// In RPC this holds the same value as `from` but, due to [`alloy::rpc::types::Transaction`]
    /// enforcing that `from` is added on top of the transaction envelope, we cannot name this field
    /// `from`. Both fields show up in RPC responses - this is expected.
    pub initiator: Address,
    /// The 160-bit address of the message call’s recipient. Cannot be missing as L1->L2 transaction cannot be `Create`.
    pub to: Address,
    /// A scalar value equal to the maximum amount of L2 gas that should be used in executing this
    /// transaction on L2. This is paid up-front before any computation is done and may not be
    /// increased later.
    #[serde(rename = "gas", with = "alloy::serde::quantity")]
    pub gas_limit: u64,
    /// Maximum amount of L2 gas that will cost to publish one byte of pubdata (every piece of data
    /// that will be stored on L1).
    #[serde(with = "alloy::serde::quantity")]
    pub gas_per_pubdata_byte_limit: u64,
    /// The absolute maximum sender is willing to pay per unit of L2 gas to get the transaction
    /// included in a block. Analog to the EIP-1559 `maxFeePerGas` for L1->L2 transactions.
    #[serde(with = "alloy::serde::quantity")]
    pub max_fee_per_gas: u128,
    /// The additional fee that is paid directly to the validator to incentivize them to include the
    /// transaction in a block. Analog to the EIP-1559 `maxPriorityFeePerGas` for L1->L2 transactions.
    #[serde(with = "alloy::serde::quantity")]
    pub max_priority_fee_per_gas: u128,
    /// Nonce of the transaction, its meaning depends on the transaction type.
    /// For priority transactions it's an operation id that is sequential for the entire chain.
    /// For genesis/upgrade transactions it's a protocol version.
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

    #[serde(skip)]
    pub marker: std::marker::PhantomData<T>,
}

mod tx_serde {
    use super::*;

    // This is the "JSON shape". It mirrors L1Tx fields PLUS the signature fields.
    // Copy over the same serde attributes so wire format matches.
    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TransactionSerdeHelper<T: L1TxType> {
        pub hash: TxHash,
        pub initiator: Address,
        pub to: Address,
        #[serde(rename = "gas", with = "alloy::serde::quantity")]
        pub gas_limit: u64,
        #[serde(with = "alloy::serde::quantity")]
        pub gas_per_pubdata_byte_limit: u64,
        #[serde(with = "alloy::serde::quantity")]
        pub max_fee_per_gas: u128,
        #[serde(with = "alloy::serde::quantity")]
        pub max_priority_fee_per_gas: u128,
        #[serde(with = "alloy::serde::quantity")]
        pub nonce: u64,
        pub value: U256,
        pub to_mint: U256,
        pub refund_recipient: Address,
        pub input: Bytes,
        pub factory_deps: Vec<B256>,
        #[serde(skip)]
        pub marker: std::marker::PhantomData<T>,

        // Extra signature fields to be compatible with standard tx JSON.
        /// ECDSA recovery id
        #[serde(with = "alloy::serde::quantity")]
        pub v: u64,
        /// ECDSA signature r
        pub r: B256,
        /// ECDSA signature s
        pub s: B256,
        /// Y-parity for EIP-2930 and EIP-1559 transactions. In theory these
        /// transactions types shouldn't have a `v` field, but in practice they
        /// are returned by nodes.
        #[serde(with = "alloy::serde::quantity")]
        pub y_parity: bool,
    }

    // Serialize: inject defaults for (r,s,v,yParity)
    impl<T: L1TxType> From<L1Tx<T>> for TransactionSerdeHelper<T> {
        fn from(tx: L1Tx<T>) -> Self {
            Self {
                hash: tx.hash,
                initiator: tx.initiator,
                to: tx.to,
                gas_limit: tx.gas_limit,
                gas_per_pubdata_byte_limit: tx.gas_per_pubdata_byte_limit,
                max_fee_per_gas: tx.max_fee_per_gas,
                max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                nonce: tx.nonce,
                value: tx.value,
                to_mint: tx.to_mint,
                refund_recipient: tx.refund_recipient,
                input: tx.input,
                factory_deps: tx.factory_deps,
                marker: std::marker::PhantomData,

                // Put defaults for signature fields
                v: 0,
                r: B256::ZERO,
                s: B256::ZERO,
                y_parity: false,
            }
        }
    }
}

impl<T: L1TxType> Typed2718 for L1Tx<T> {
    fn ty(&self) -> u8 {
        T::TX_TYPE
    }
}

impl<T: L1TxType> RlpEcdsaEncodableTx for L1Tx<T> {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.hash.length()
            + self.initiator.length()
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
        self.initiator.encode(out);
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

impl<T: L1TxType> RlpEcdsaDecodableTx for L1Tx<T> {
    const DEFAULT_TX_TYPE: u8 = T::TX_TYPE;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            hash: Decodable::decode(buf)?,
            initiator: Decodable::decode(buf)?,
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
            marker: std::marker::PhantomData,
        })
    }
}

impl<T: L1TxType> Encodable for L1Tx<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl<T: L1TxType> Decodable for L1Tx<T> {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

impl<T: L1TxType> Transaction for L1Tx<T> {
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

    fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
        // At the moment `max_fee_per_gas` is the effective gas price for L1 txs.
        self.max_fee_per_gas
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

/// Transaction envelope for L1->L2 transactions. Mostly needed as an intermediary level for `ZkEnvelope`.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct L1Envelope<T: L1TxType> {
    #[serde(flatten)]
    pub inner: L1Tx<T>,
}

impl<T: L1TxType> L1Envelope<T> {
    pub fn hash(&self) -> &B256 {
        &self.inner.hash
    }

    pub fn priority_id(&self) -> L1TxSerialId {
        self.inner.nonce
    }
}

impl<T: L1TxType> Transaction for L1Envelope<T> {
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

impl<T: L1TxType> Typed2718 for L1Envelope<T> {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl<T: L1TxType> Decodable2718 for L1Envelope<T> {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        let decoded = L1Tx::rlp_decode_signed(buf)?;

        if decoded.ty() != ty {
            return Err(Eip2718Error::UnexpectedType(ty));
        }

        Ok(Self {
            inner: decoded.into_parts().0,
        })
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        // Do not try to decode untyped transactions
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl<T: L1TxType> Encodable2718 for L1Envelope<T> {
    fn encode_2718_len(&self) -> usize {
        self.inner
            .eip2718_encoded_length(&Signature::new(U256::ZERO, U256::ZERO, false))
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner
            .eip2718_encode(&Signature::new(U256::ZERO, U256::ZERO, false), out)
    }
}

impl<T: L1TxType> Decodable for L1Envelope<T> {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let decoded = Signed::<L1Tx<T>>::decode_2718(buf)?;
        Ok(L1Envelope {
            inner: decoded.into_parts().0,
        })
    }
}

impl<T: L1TxType> Encodable for L1Envelope<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out)
    }
}

impl<T: L1TxType> TryFrom<L2CanonicalTransaction> for L1Envelope<T> {
    type Error = L1EnvelopeError;

    fn try_from(tx: L2CanonicalTransaction) -> Result<Self, Self::Error> {
        let tx_type = tx.txType.saturating_to();
        if tx_type != T::TX_TYPE {
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
        let inner = L1Tx {
            hash,
            initiator: Address::from_slice(&tx.from.to_be_bytes::<32>()[12..]),
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
            marker: std::marker::PhantomData,
        };
        Ok(L1Envelope { inner })
    }
}

impl TryFrom<NewPriorityRequest> for L1Envelope<L1PriorityTxType> {
    type Error = L1EnvelopeError;

    fn try_from(value: NewPriorityRequest) -> Result<Self, Self::Error> {
        value.transaction.try_into()
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

#[cfg(test)]
mod tests {
    use crate::{L1PriorityEnvelope, L1Tx, ZkEnvelope};
    use alloy::consensus::transaction::Recovered;
    use alloy::primitives::{address, b256, bytes};
    use alloy::sol_types::private::u256;

    #[test]
    fn l1_rpc_json() {
        // Test that L1 tx (de)serializes to a reasonable JSON representation
        let l1_tx_json = serde_json::json!({
          "type": "0x7f",
          "hash": "0x4164624346d4c915977debf68dbd721f8ae86b964080925aecf6911dd47a6ece",
          "initiator": "0x357fe6c9f85dc429596577cf2e7a191f60b6865b",
          "to": "0x357fe6c9f85dc429596577cf2e7a191f60b6865b",
          "gas": "0x493e0",
          "gasPerPubdataByteLimit": "0x320",
          "maxFeePerGas": "0xee6fcf4",
          "maxPriorityFeePerGas": "0x0",
          "nonce": "0x1",
          "value": "0x32",
          "toMint": "0x4b08a6610e32",
          "refundRecipient": "0x357fe6c9f85dc429596577cf2e7a191f60b6865b",
          "input": "0x",
          "factoryDeps": [],
          "blockHash": "0xb88bcbb8c3d67a79e4330b9410fb613f5d4d13747cdc80747b4c29ad32dbdfcc",
          "blockNumber": "0x3",
          "transactionIndex": "0x0",
          "from": "0x357fe6c9f85dc429596577cf2e7a191f60b6865b",
          "gasPrice": "0xee6fcf4",
          "v": "0x0",
          "r": "0x0000000000000000000000000000000000000000000000000000000000000000",
          "s": "0x0000000000000000000000000000000000000000000000000000000000000000",
          "yParity": "0x0",
        });
        let l1_tx: alloy::rpc::types::Transaction<ZkEnvelope> =
            serde_json::from_value(l1_tx_json.clone()).unwrap();

        let expected_signer = address!("0x357fe6c9f85dc429596577cf2e7a191f60b6865b");
        let expected_envelope = ZkEnvelope::L1(L1PriorityEnvelope {
            inner: L1Tx {
                hash: b256!("0x4164624346d4c915977debf68dbd721f8ae86b964080925aecf6911dd47a6ece"),
                initiator: expected_signer,
                to: expected_signer,
                gas_limit: 0x493e0,
                gas_per_pubdata_byte_limit: 0x320,
                max_fee_per_gas: 0xee6fcf4,
                max_priority_fee_per_gas: 0x0,
                nonce: 0x1,
                value: u256(0x32),
                to_mint: u256(0x4b08a6610e32),
                refund_recipient: expected_signer,
                input: bytes!("0x"),
                factory_deps: vec![],
                marker: Default::default(),
            },
        });
        let expected_l1_tx = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(expected_envelope, expected_signer),
            block_hash: Some(b256!(
                "0xb88bcbb8c3d67a79e4330b9410fb613f5d4d13747cdc80747b4c29ad32dbdfcc"
            )),
            block_number: Some(0x3),
            transaction_index: Some(0x0),
            effective_gas_price: Some(0xee6fcf4),
        };
        assert_eq!(l1_tx, expected_l1_tx);

        let roundback_l1_tx_json = serde_json::to_value(&l1_tx).unwrap();
        assert_eq!(roundback_l1_tx_json, l1_tx_json);
    }
}
