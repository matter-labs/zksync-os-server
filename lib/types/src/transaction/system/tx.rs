use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::consensus::{Transaction, Typed2718};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::primitives::{ChainId, keccak256};
use alloy::rpc::types::{AccessList, SignedAuthorization};
use alloy_rlp::{BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};

use crate::transaction::SystemTxType;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase", into = "tx_serde::TransactionSerdeHelper<T>")]
pub struct SystemTransaction<T: SystemTxType> {
    #[serde(rename = "gas", with = "alloy::serde::quantity")]
    pub gas_limit: u64,
    pub to: Address,
    pub input: Bytes,

    #[serde(skip)]
    pub marker: std::marker::PhantomData<T>,
}

impl<T: SystemTxType> SystemTransaction<T> {
    pub fn calculate_hash(&self) -> B256 {
        keccak256(self.encoded_2718())
    }
}

mod tx_serde {
    use alloy::primitives::TxHash;

    use super::*;
    use crate::transaction::BOOTLOADER_FORMAL_ADDRESS;

    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TransactionSerdeHelper<T: SystemTxType> {
        pub hash: TxHash,
        pub initiator: Address,
        pub to: Address,
        #[serde(rename = "gas", with = "alloy::serde::quantity")]
        pub gas_limit: u64,
        #[serde(with = "alloy::serde::quantity")]
        pub max_fee_per_gas: u128,
        #[serde(with = "alloy::serde::quantity")]
        pub max_priority_fee_per_gas: u128,
        #[serde(with = "alloy::serde::quantity")]
        pub nonce: u64,
        pub value: U256,
        pub input: Bytes,
        #[serde(skip)]
        pub marker: std::marker::PhantomData<T>,

        #[serde(with = "alloy::serde::quantity")]
        pub v: u64,
        pub r: B256,
        pub s: B256,
        #[serde(with = "alloy::serde::quantity")]
        pub y_parity: bool,
    }

    // Serialize: inject defaults for (r,s,v,yParity)
    impl<T: SystemTxType> From<SystemTransaction<T>> for TransactionSerdeHelper<T> {
        fn from(tx: SystemTransaction<T>) -> Self {
            Self {
                hash: tx.calculate_hash(),
                initiator: BOOTLOADER_FORMAL_ADDRESS,
                to: tx.to,
                gas_limit: tx.gas_limit,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: 0,
                value: U256::ZERO,
                input: tx.input,
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

impl<T: SystemTxType> Transaction for SystemTransaction<T> {
    fn chain_id(&self) -> Option<ChainId> {
        None
    }

    fn nonce(&self) -> u64 {
        0
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    fn gas_price(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_gas(&self) -> u128 {
        0
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(0)
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        0
    }

    fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
        0
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
        U256::ZERO
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

impl<T: SystemTxType> Typed2718 for SystemTransaction<T> {
    fn ty(&self) -> u8 {
        T::TX_TYPE
    }
}

impl<T: SystemTxType> Encodable2718 for SystemTransaction<T> {
    fn encode_2718_len(&self) -> usize {
        1 + self.length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        let mut rlp_body = Vec::new();
        Encodable::encode(&self, &mut rlp_body);
        out.put_u8(T::TX_TYPE);
        out.put_slice(&rlp_body);
    }
}

impl<T: SystemTxType> RlpEcdsaEncodableTx for SystemTransaction<T> {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.gas_limit.length() + self.to.length() + self.input.length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.input.encode(out);
    }
}

impl<T: SystemTxType> RlpEcdsaDecodableTx for SystemTransaction<T> {
    const DEFAULT_TX_TYPE: u8 = T::TX_TYPE;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            gas_limit: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,

            marker: std::marker::PhantomData,
        })
    }
}

// if something goes wrong with encoding, there's a chance that something is wrong here
impl<T: SystemTxType> Encodable for SystemTransaction<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}
