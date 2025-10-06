use crate::transaction::TxType;
use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::consensus::{SignableTransaction, Signed, Transaction, Typed2718};
use alloy::eips::eip2718::IsTyped2718;
use alloy::eips::eip2930::AccessList;
use alloy::eips::eip7702::SignedAuthorization;
use alloy::primitives::bytes::BufMut;
use alloy::primitives::{
    Address, B256, Bytes, ChainId, Keccak256, Signature, TxKind, U256, keccak256,
};
use alloy_rlp::{Decodable, Encodable, Header};
use serde::{Deserialize, Serialize};
use std::mem;

/// Identifier for an EIP712 transaction.
pub const EIP712_TX_TYPE_ID: u8 = 113;

/// A custom EIP-712 transaction specific to ZKsync OS.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxEip712 {
    /// EIP-155: Simple replay attack protection
    #[serde(with = "alloy::serde::quantity")]
    pub chain_id: ChainId,
    /// A scalar value equal to the number of transactions sent by the sender; formally Tn.
    #[serde(with = "alloy::serde::quantity")]
    pub nonce: u64,
    /// A scalar value equal to the maximum
    /// amount of gas that should be used in executing
    /// this transaction. This is paid up-front, before any
    /// computation is done and may not be increased
    /// later; formally Tg.
    #[serde(with = "alloy::serde::quantity", rename = "gas", alias = "gasLimit")]
    pub gas_limit: u64,
    /// A scalar value equal to the maximum
    /// amount of gas that should be used in executing
    /// this transaction. This is paid up-front, before any
    /// computation is done and may not be increased
    /// later; formally Tg.
    ///
    /// As ethereum circulation is around 120mil eth as of 2022 that is around
    /// 120000000000000000000000000 wei we are safe to use u128 as its max number is:
    /// 340282366920938463463374607431768211455
    ///
    /// This is also known as `GasFeeCap`
    #[serde(with = "alloy::serde::quantity")]
    pub max_fee_per_gas: u128,
    /// Max Priority fee that transaction is paying
    ///
    /// As ethereum circulation is around 120mil eth as of 2022 that is around
    /// 120000000000000000000000000 wei we are safe to use u128 as its max number is:
    /// 340282366920938463463374607431768211455
    ///
    /// This is also known as `GasTipCap`
    #[serde(with = "alloy::serde::quantity")]
    pub max_priority_fee_per_gas: u128,
    /// Max gas per pubdata that transaction is paying.
    #[serde(with = "alloy::serde::quantity")]
    pub gas_per_pubdata: u64,
    /// The 160-bit address of the message call’s origin.
    pub from: Address,
    /// The 160-bit address of the message call’s recipient.
    pub to: Address,
    /// A scalar value equal to the number of Wei to
    /// be transferred to the message call’s recipient or,
    /// in the case of contract creation, as an endowment
    /// to the newly created account; formally Tv.
    pub value: U256,
    /// Input has two uses depending if `to` field is Create or Call.
    /// pub init: An unlimited size byte array specifying the
    /// EVM-code for the account initialisation procedure CREATE,
    /// data: An unlimited size byte array specifying the
    /// input data of the message call, formally Td.
    pub input: Bytes,
    pub signature: Option<Signature>,
    /// LEGACY. The 160-bit address of the message call's paymaster. Must be empty.
    #[serde(default)]
    pub paymaster: Address,
    /// LEGACY. Input to be provided to the paymaster. Must be empty.
    #[serde(default)]
    pub paymaster_input: Bytes,
    /// LEGACY. Factory dependencies. Must be empty.
    #[serde(default)]
    pub factory_deps: Vec<Bytes>,
}

impl TxEip712 {
    /// Get the transaction type
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> TxType {
        TxType::Eip712
    }

    /// Calculates a heuristic for the in-memory size of the [TxEip1559]
    /// transaction.
    #[inline]
    pub fn size(&self) -> usize {
        mem::size_of::<ChainId>() + // chain_id
            mem::size_of::<u64>() + // nonce
            mem::size_of::<u64>() + // gas_limit
            mem::size_of::<u128>() + // max_fee_per_gas
            mem::size_of::<u128>() + // max_priority_fee_per_gas
            mem::size_of::<u64>() + // gas_per_pubdata
            mem::size_of::<Address>() + // from
            mem::size_of::<Address>() + // to
            mem::size_of::<U256>() + // value
            self.input.len() + // input
            mem::size_of::<Address>() + // paymaster
            self.paymaster_input.len() + // paymaster_input
            self.factory_deps.iter().map(|dep| dep.len()).sum::<usize>() // factory_deps
    }
}

impl TxEip712 {
    // Keccak256 of:
    // EIP712Domain(string name,string version,uint256 chainId)
    // = c2f8787176b8ac6bf7215b4adcc1e069bf4ab82d9ab1df05a57a91d425935b6e
    const DOMAIN_TYPE_HASH: [u8; 32] = [
        0xc2, 0xf8, 0x78, 0x71, 0x76, 0xb8, 0xac, 0x6b, 0xf7, 0x21, 0x5b, 0x4a, 0xdc, 0xc1, 0xe0,
        0x69, 0xbf, 0x4a, 0xb8, 0x2d, 0x9a, 0xb1, 0xdf, 0x05, 0xa5, 0x7a, 0x91, 0xd4, 0x25, 0x93,
        0x5b, 0x6e,
    ];

    // Keccak256 of:
    // zkSync
    // = 19b453ce45aaaaf3a300f5a9ec95869b4f28ab10430b572ee218c3a6a5e07d6f
    const DOMAIN_NAME_HASH: [u8; 32] = [
        0x19, 0xb4, 0x53, 0xce, 0x45, 0xaa, 0xaa, 0xf3, 0xa3, 0x00, 0xf5, 0xa9, 0xec, 0x95, 0x86,
        0x9b, 0x4f, 0x28, 0xab, 0x10, 0x43, 0x0b, 0x57, 0x2e, 0xe2, 0x18, 0xc3, 0xa6, 0xa5, 0xe0,
        0x7d, 0x6f,
    ];

    // Keccak256 of:
    // 2
    // = ad7c5bef027816a800da1736444fb58a807ef4c9603b7848673f7e3a68eb14a5
    const DOMAIN_VERSION_HASH: [u8; 32] = [
        0xad, 0x7c, 0x5b, 0xef, 0x02, 0x78, 0x16, 0xa8, 0x00, 0xda, 0x17, 0x36, 0x44, 0x4f, 0xb5,
        0x8a, 0x80, 0x7e, 0xf4, 0xc9, 0x60, 0x3b, 0x78, 0x48, 0x67, 0x3f, 0x7e, 0x3a, 0x68, 0xeb,
        0x14, 0xa5,
    ];

    fn domain_hash_struct(&self) -> B256 {
        let mut hasher = Keccak256::new();
        hasher.update(Self::DOMAIN_TYPE_HASH);
        hasher.update(Self::DOMAIN_NAME_HASH);
        hasher.update(Self::DOMAIN_VERSION_HASH);
        hasher.update(U256::from(self.chain_id).to_be_bytes::<32>());
        hasher.finalize()
    }

    // Keccak256 of:
    // Transaction(uint256 txType,uint256 from,uint256 to,uint256 gasLimit,uint256 gasPerPubdataByteLimit,uint256 maxFeePerGas,uint256 maxPriorityFeePerGas,uint256 paymaster,uint256 nonce,uint256 value,bytes data,bytes32[] factoryDeps,bytes paymasterInput)
    // = 848e1bfa1ac4e3576b728bda6721b215c70a7799a5b4866282a71bab954baac8
    const TYPE_HASH: [u8; 32] = [
        0x84, 0x8e, 0x1b, 0xfa, 0x1a, 0xc4, 0xe3, 0x57, 0x6b, 0x72, 0x8b, 0xda, 0x67, 0x21, 0xb2,
        0x15, 0xc7, 0x0a, 0x77, 0x99, 0xa5, 0xb4, 0x86, 0x62, 0x82, 0xa7, 0x1b, 0xab, 0x95, 0x4b,
        0xaa, 0xc8,
    ];

    fn hash_struct(&self) -> B256 {
        let mut hasher = Keccak256::new();
        hasher.update(Self::TYPE_HASH);
        hasher.update(EIP712_TX_TYPE_ID.to_be_bytes());
        hasher.update(self.from);
        hasher.update(self.to);
        hasher.update(self.gas_limit.to_be_bytes());
        hasher.update(self.gas_per_pubdata.to_be_bytes());
        hasher.update(self.max_fee_per_gas.to_be_bytes());
        hasher.update(self.max_priority_fee_per_gas.to_be_bytes());
        hasher.update(self.paymaster);
        hasher.update(self.nonce.to_be_bytes());
        hasher.update(self.value.to_be_bytes::<32>());

        let data_hash = keccak256(&self.input);
        hasher.update(data_hash);

        // assume empty factory deps
        let factory_deps_hash = keccak256([]);
        hasher.update(factory_deps_hash);

        let paymaster_input_hash = keccak256(&self.paymaster_input);
        hasher.update(paymaster_input_hash);

        hasher.finalize()
    }

    fn fields_len(&self) -> usize {
        self.nonce.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + self.to.length()
            + self.value.length()
            + self.input.length()
            + self.chain_id.length()
            + self.from.length()
            + self.gas_per_pubdata.length()
            + self.factory_deps.length()
    }
}

impl RlpEcdsaEncodableTx for TxEip712 {
    fn rlp_encoded_fields_length(&self) -> usize {
        let payload_length = self.fields_len()
            + self.signature.unwrap().rlp_rs_len()
            + self.signature.unwrap().v().length();
        Header {
            list: true,
            payload_length,
        }
        .length()
            + payload_length
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        let signature = self.signature.as_ref().unwrap();
        self.rlp_encode_signed(signature, out);
    }

    fn rlp_encode_signed(&self, signature: &Signature, out: &mut dyn BufMut) {
        let payload_length = self.fields_len() + signature.rlp_rs_len() + signature.v().length();
        let header = Header {
            list: true,
            payload_length,
        };
        header.encode(out);

        self.nonce.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        signature.write_rlp_vrs(out, signature.v());
        self.chain_id.encode(out);
        self.from.encode(out);
        self.gas_per_pubdata.encode(out);
        self.factory_deps.encode(out);
    }
}

impl RlpEcdsaDecodableTx for TxEip712 {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() as u8 };

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::rlp_decode_signed(buf).map(|signed| signed.into_parts().0)
    }

    fn rlp_decode_signed(buf: &mut &[u8]) -> alloy_rlp::Result<Signed<Self>> {
        fn opt_decode<T: Decodable>(buf: &mut &[u8]) -> alloy::rlp::Result<Option<T>> {
            Ok(Decodable::decode(buf).ok()) // TODO: better validation of error?
        }

        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy::rlp::Error::UnexpectedString);
        }

        // record original length so we can check encoding
        let original_len = buf.len();

        let nonce: U256 = Decodable::decode(buf)?;
        let max_priority_fee_per_gas = Decodable::decode(buf)?;
        let max_fee_per_gas = Decodable::decode(buf)?;
        let gas_limit = Decodable::decode(buf)?;
        let to = Decodable::decode(buf)?;
        let value = Decodable::decode(buf)?;
        let input = Decodable::decode(buf)?;
        let signature = Signature::decode_rlp_vrs(buf, bool::decode)?;
        let chain_id = Decodable::decode(buf)?;
        let from = Decodable::decode(buf)?;
        let gas_per_pubdata: U256 = Decodable::decode(buf)?;
        let factory_deps = Decodable::decode(buf)?;
        // todo: assert these are empty
        let _custom_signature: Option<Bytes> = opt_decode(buf)?;
        let _paymaster: Option<PaymasterParams> = opt_decode(buf)?;

        let tx = Self {
            chain_id,
            nonce: nonce.saturating_to(),
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to,
            from,
            value,
            input,
            signature: Some(signature),
            gas_per_pubdata: gas_per_pubdata.saturating_to(),
            factory_deps,
            paymaster: Address::ZERO,
            paymaster_input: Bytes::new(),
        };

        let signed = tx.into_signed(signature);

        if buf.len() + header.payload_length != original_len {
            return Err(alloy::rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: original_len - buf.len(),
            });
        }

        Ok(signed)
    }
}

impl Transaction for TxEip712 {
    #[inline]
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    #[inline]
    fn nonce(&self) -> u64 {
        self.nonce
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    #[inline]
    fn gas_price(&self) -> Option<u128> {
        None
    }

    #[inline]
    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    #[inline]
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    #[inline]
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    #[inline]
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

    #[inline]
    fn is_dynamic_fee(&self) -> bool {
        true
    }

    #[inline]
    fn kind(&self) -> TxKind {
        TxKind::Call(self.to)
    }

    #[inline]
    fn is_create(&self) -> bool {
        false
    }

    #[inline]
    fn value(&self) -> U256 {
        self.value
    }

    #[inline]
    fn input(&self) -> &Bytes {
        &self.input
    }

    #[inline]
    fn access_list(&self) -> Option<&AccessList> {
        None
    }

    #[inline]
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    #[inline]
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl Typed2718 for TxEip712 {
    fn ty(&self) -> u8 {
        TxType::Eip712 as u8
    }
}

impl IsTyped2718 for TxEip712 {
    fn is_type(type_id: u8) -> bool {
        matches!(type_id, 0x71)
    }
}

impl SignableTransaction<Signature> for TxEip712 {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        out.put_u8(Self::tx_type() as u8);
        self.encode(out)
    }

    fn payload_len_for_signature(&self) -> usize {
        self.length() + 1
    }

    fn signature_hash(&self) -> B256 {
        // Calculate signed tx hash(the one that should be signed by the sender):
        // Keccak256(0x19 0x01 ‖ domainSeparator ‖ hashStruct(tx))
        let domain_separator = self.domain_hash_struct();
        let hs = self.hash_struct();
        let mut hasher = Keccak256::new();
        hasher.update([0x19, 0x01]);
        hasher.update(domain_separator);
        hasher.update(hs);

        hasher.finalize()
    }
}

impl Encodable for TxEip712 {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxEip712 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

#[cfg(feature = "reth")]
impl reth_primitives_traits::InMemorySize for TxEip712 {
    #[inline]
    fn size(&self) -> usize {
        Self::size(self)
    }
}

// Seralize 'Bytes' to encode them into RLP friendly format.
fn serialize_bytes_custom<S: serde::Serializer>(
    bytes: &Bytes,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(&bytes.0)
}

/// Represents the paymaster parameters for ZKsync Era transactions.
#[derive(Default, Serialize, Deserialize, Clone, PartialEq, Debug, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct PaymasterParams {
    /// Address of the paymaster.
    pub paymaster: Address,
    /// Paymaster input.
    // A custom serialization is needed (otherwise RLP treats it as string).
    #[serde(serialize_with = "serialize_bytes_custom")]
    pub paymaster_input: Bytes,
}

impl Decodable for PaymasterParams {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let mut bytes = Header::decode_bytes(buf, true)?;
        let payload_view = &mut bytes;
        Ok(Self {
            paymaster: dbg!(Decodable::decode(payload_view))?,
            paymaster_input: dbg!(Decodable::decode(payload_view))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TxEip712;
    use alloy::consensus::transaction::RlpEcdsaDecodableTx;
    use alloy::hex::FromHex;
    use alloy::primitives::{Bytes, U256, address, hex};

    #[test]
    fn decode_eip712_tx() {
        // Does not have type byte.
        let encoded = hex::decode("f8b701800b0c940754b07d1ea3071c3ec9bd86b2aa6f1a59a514980a8301020380a0635f9ee3a1523de15fc8b72a0eea12f5247c6b6e2369ed158274587af6496599a030f7c66d1ed24fca92527e6974b85b07ec30fdd5c2d41eae46966224add965f982010e9409a6aa96b9a17d7f7ba3e3b19811c082aba9f1e304e1a0020202020202020202020202020202020202020202020202020202020202020283010203d694000000000000000000000000000000000000000080").unwrap();
        let signed_tx = TxEip712::rlp_decode_signed(&mut &encoded[..]).unwrap();
        let tx = signed_tx.tx();
        assert_eq!(tx.chain_id, 270);
        assert_eq!(tx.nonce, 1);
        assert_eq!(tx.gas_limit, 12);
        assert_eq!(tx.max_fee_per_gas, 11);
        assert_eq!(tx.max_priority_fee_per_gas, 0);
        assert_eq!(tx.to, address!("0754b07d1ea3071c3ec9bd86b2aa6f1a59a51498"));
        assert_eq!(
            tx.from,
            address!("09a6aa96b9a17d7f7ba3e3b19811c082aba9f1e3")
        );
        assert_eq!(tx.value, U256::from(10));
        assert_eq!(tx.input, Bytes::from_hex("0x010203").unwrap());
        assert_eq!(tx.gas_per_pubdata, 4);
    }
}
