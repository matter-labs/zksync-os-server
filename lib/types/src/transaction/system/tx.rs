use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::consensus::{Transaction, Typed2718};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::primitives::{ChainId, keccak256};
use alloy::rpc::types::{AccessList, SignedAuthorization};
use alloy_rlp::{BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};

use crate::transaction::SYSTEM_TX_TYPE_ID;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemTx {
    pub to: Address,
    pub input: Bytes,
}

impl SystemTx {
    pub fn calculate_hash(&self) -> B256 {
        keccak256(self.encoded_2718())
    }
}

impl Transaction for SystemTx {
    fn chain_id(&self) -> Option<ChainId> {
        None
    }

    fn nonce(&self) -> u64 {
        0
    }

    fn gas_limit(&self) -> u64 {
        0
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

impl Typed2718 for SystemTx {
    fn ty(&self) -> u8 {
        SYSTEM_TX_TYPE_ID
    }
}

impl Encodable2718 for SystemTx {
    fn encode_2718_len(&self) -> usize {
        1 + self.length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        let mut rlp_body = Vec::new();
        Encodable::encode(&self, &mut rlp_body);
        out.put_u8(SYSTEM_TX_TYPE_ID);
        out.put_slice(&rlp_body);
    }
}

impl RlpEcdsaEncodableTx for SystemTx {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.to.length() + self.input.length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.to.encode(out);
        self.input.encode(out);
    }
}

impl RlpEcdsaDecodableTx for SystemTx {
    const DEFAULT_TX_TYPE: u8 = SYSTEM_TX_TYPE_ID;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            to: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
        })
    }
}

impl Encodable for SystemTx {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}
