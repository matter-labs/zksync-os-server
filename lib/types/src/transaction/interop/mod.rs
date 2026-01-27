use crate::transaction::Transaction;
use crate::transaction::tx::InteropRootsTx;
use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::eips::eip2718::{Eip2718Error, Eip2718Result};
use alloy::eips::{Decodable2718, Encodable2718, Typed2718};
use alloy::primitives::ChainId;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy::rpc::types::{AccessList, SignedAuthorization};
use alloy::sol_types::SolCall;
use alloy_rlp::{BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::{IMessageRoot::addInteropRootCall, InteropRoot};

pub mod tx;

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");

pub const INTEROP_ROOTS_TX_TYPE_ID: u8 = 125;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
#[serde(into = "tx_serde::TransactionSerdeHelper")]
pub struct InteropRootsEnvelope {
    /// Hash of the transaction
    /// Stored in an envelope and calculated separately from transaction as hash of transaction is not part of transaction itself.
    pub hash: B256,
    pub inner: InteropRootsTx,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct IndexedInteropRootsEnvelope {
    pub log_index: InteropRootsLogIndex,
    pub envelope: InteropRootsEnvelope,
}

mod tx_serde {
    use alloy::primitives::TxHash;

    use super::*;
    use crate::transaction::BOOTLOADER_FORMAL_ADDRESS;

    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TransactionSerdeHelper {
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

        #[serde(with = "alloy::serde::quantity")]
        pub v: u64,
        pub r: B256,
        pub s: B256,
        #[serde(with = "alloy::serde::quantity")]
        pub y_parity: bool,
    }

    // Serialize: inject defaults for (r,s,v,yParity)
    impl From<InteropRootsEnvelope> for TransactionSerdeHelper {
        fn from(tx: InteropRootsEnvelope) -> Self {
            Self {
                hash: *tx.hash(),
                initiator: BOOTLOADER_FORMAL_ADDRESS,
                to: L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS,
                gas_limit: tx.gas_limit(),
                max_fee_per_gas: tx.max_fee_per_gas(),
                max_priority_fee_per_gas: tx.max_priority_fee_per_gas().unwrap_or(0),
                nonce: tx.nonce(),
                value: tx.value(),
                input: Bytes::from(tx.input().to_vec()),
                // Put defaults for signature fields
                v: 0,
                r: B256::ZERO,
                s: B256::ZERO,
                y_parity: false,
            }
        }
    }
}

/// A helper struct to store the block number and index in block of published interop roots event.
#[derive(Default, Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq, PartialOrd)]
pub struct InteropRootsLogIndex {
    /// Block number from which event was published.
    pub block_number: u64,
    /// Index of the event in the block.
    pub index_in_block: u64,
}

impl Encodable for InteropRootsLogIndex {
    fn encode(&self, out: &mut dyn BufMut) {
        self.block_number.encode(out);
        self.index_in_block.encode(out);
    }

    fn length(&self) -> usize {
        self.block_number.length() + self.index_in_block.length()
    }
}

impl Decodable for InteropRootsLogIndex {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            block_number: Decodable::decode(buf)?,
            index_in_block: Decodable::decode(buf)?,
        })
    }
}

impl InteropRootsEnvelope {
    pub fn from_interop_roots(interop_roots: Vec<InteropRoot>) -> Self {
        assert_eq!(
            interop_roots.len(),
            1,
            "Sequencer doesn't support multiple interop roots in single transaction yet"
        );

        let calldata = addInteropRootCall {
            chainId: interop_roots[0].chainId,
            blockOrBatchNumber: interop_roots[0].blockOrBatchNumber,
            sides: interop_roots[0].sides.clone(),
        }
        .abi_encode();

        let transaction = InteropRootsTx {
            to: L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS,
            input: Bytes::from(calldata),
        };

        Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
        }
    }

    pub fn interop_roots_count(&self) -> u64 {
        // for now it is always expected to be 1, until we support multiple interop roots in single transaction
        1
    }

    pub fn hash(&self) -> &B256 {
        &self.hash
    }
}

impl Typed2718 for InteropRootsEnvelope {
    fn ty(&self) -> u8 {
        INTEROP_ROOTS_TX_TYPE_ID
    }
}

impl RlpEcdsaEncodableTx for InteropRootsEnvelope {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.inner.rlp_encoded_fields_length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for InteropRootsEnvelope {
    const DEFAULT_TX_TYPE: u8 = INTEROP_ROOTS_TX_TYPE_ID;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let transaction = InteropRootsTx::rlp_decode_fields(buf)?;
        Ok(Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
        })
    }
}

impl Encodable for InteropRootsEnvelope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out);
    }

    fn length(&self) -> usize {
        self.inner.length()
    }
}

impl Encodable2718 for InteropRootsEnvelope {
    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner.encode_2718(out);
    }
}

impl Decodable2718 for InteropRootsEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != INTEROP_ROOTS_TX_TYPE_ID {
            return Err(Eip2718Error::UnexpectedType(ty));
        }

        let transaction = InteropRootsTx::rlp_decode(buf)
            .map_err(|_| Eip2718Error::RlpError(alloy::rlp::Error::Custom("decode failed")))?;

        let hash = transaction.calculate_hash();

        Ok(Self {
            hash,
            inner: transaction,
        })
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        // Do not try to decode untyped transactions
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Transaction for InteropRootsEnvelope {
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

#[cfg(test)]
mod tests {
    use crate::InteropRootsEnvelope;
    use crate::transaction::tx::InteropRootsTx;

    #[test]
    fn interop_roots_tx_serialization() {
        // Interop roots serialization should be consistent with Ethereum JSON-RPC spec
        // See https://ethereum.github.io/execution-apis/api-documentation/

        let transaction = InteropRootsTx {
            to: Default::default(),
            input: Default::default(),
        };

        let tx = InteropRootsEnvelope {
            hash: transaction.calculate_hash(),
            inner: transaction,
        };

        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0x0b5cf6f6f3b9deb0fd6cb66f51e15f4d751e0724401c2cd7b7df59489fe5f289",
  "initiator": "0x0000000000000000000000000000000000008001",
  "to": "0x0000000000000000000000000000000000010008",
  "gas": "0x0",
  "maxFeePerGas": "0x0",
  "maxPriorityFeePerGas": "0x0",
  "nonce": "0x0",
  "value": "0x0",
  "input": "0x",
  "v": "0x0",
  "r": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "s": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "yParity": "0x0"
}"#
        );
    }
}
