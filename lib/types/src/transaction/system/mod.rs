use crate::transaction::Transaction;
use crate::transaction::tx::SystemTx;
use crate::transaction::utils::SystemTxInput;
use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::eips::eip2718::{Eip2718Error, Eip2718Result};
use alloy::eips::{Decodable2718, Encodable2718, Typed2718};
use alloy::primitives::ChainId;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::{AccessList, SignedAuthorization};
use alloy_rlp::{BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use zksync_os_contract_interface::InteropRoot;

pub mod tx;
pub mod utils;
pub use utils::{L2_INTEROP_ROOT_STORAGE_ADDRESS, SYSTEM_TX_TYPE_ID, SystemTxType};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(into = "tx_serde::TransactionSerdeHelper")]
pub struct SystemTxEnvelope {
    /// Hash of the transaction
    /// Stored in an envelope and calculated separately from transaction as hash of transaction is not part of transaction itself.
    hash: B256,
    inner: SystemTx,
    #[serde(skip)]
    subtype: OnceLock<SystemTxType>,
}

impl PartialEq for SystemTxEnvelope {
    fn eq(&self, other: &Self) -> bool {
        self.hash() == other.hash()
            && self.inner == other.inner
            && self.system_subtype() == other.system_subtype()
    }
}

impl SystemTxEnvelope {
    /// A constructor for system transaction that imports interop roots
    pub fn import_interop_roots(roots: Vec<InteropRoot>) -> Self {
        Self::create_from_input(SystemTxInput::ImportInteropRoots(roots))
    }

    /// A constructor for system transaction that sets the settlement layer chain id
    pub fn set_sl_chain_id(chain_id: ChainId) -> Self {
        Self::create_from_input(SystemTxInput::SetSLChainId(chain_id))
    }

    fn create_from_input(tx_input: SystemTxInput) -> Self {
        let calldata = tx_input.abi_encode();

        let transaction = SystemTx {
            to: tx_input.to_address(),
            input: Bytes::from(calldata),
        };

        Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
            subtype: OnceLock::new(),
        }
    }

    pub fn system_subtype(&self) -> &SystemTxType {
        self.subtype.get_or_init(|| {
            let input = SystemTxInput::abi_decode(self.inner.input());
            assert_eq!(self.to(), Some(input.to_address()));
            match input {
                SystemTxInput::ImportInteropRoots(roots) => {
                    SystemTxType::ImportInteropRoots(roots.len() as u64)
                }
                SystemTxInput::SetSLChainId(_) => SystemTxType::SetSLChainId,
            }
        })
    }

    pub fn hash(&self) -> &B256 {
        &self.hash
    }
}

#[derive(Clone, Debug)]
pub struct IndexedInteropRoot {
    pub log_index: InteropRootsLogIndex,
    pub root: InteropRoot,
}

mod tx_serde {
    use alloy::primitives::TxHash;

    use super::*;
    use crate::transaction::utils::BOOTLOADER_FORMAL_ADDRESS;

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
    impl From<SystemTxEnvelope> for TransactionSerdeHelper {
        fn from(tx: SystemTxEnvelope) -> Self {
            let tx_input = SystemTxInput::abi_decode(tx.inner.input());
            Self {
                hash: *tx.hash(),
                initiator: BOOTLOADER_FORMAL_ADDRESS,
                to: tx_input.to_address(),
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

impl Typed2718 for SystemTxEnvelope {
    fn ty(&self) -> u8 {
        SYSTEM_TX_TYPE_ID
    }
}

impl RlpEcdsaEncodableTx for SystemTxEnvelope {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.inner.rlp_encoded_fields_length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for SystemTxEnvelope {
    const DEFAULT_TX_TYPE: u8 = SYSTEM_TX_TYPE_ID;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let transaction = SystemTx::rlp_decode_fields(buf)?;
        Ok(Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
            subtype: OnceLock::new(),
        })
    }
}

impl Encodable for SystemTxEnvelope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out);
    }

    fn length(&self) -> usize {
        self.inner.length()
    }
}

impl Encodable2718 for SystemTxEnvelope {
    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner.encode_2718(out);
    }
}

impl Decodable2718 for SystemTxEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != SYSTEM_TX_TYPE_ID {
            return Err(Eip2718Error::UnexpectedType(ty));
        }

        let transaction = SystemTx::rlp_decode(buf)
            .map_err(|_| Eip2718Error::RlpError(alloy::rlp::Error::Custom("decode failed")))?;

        let hash = transaction.calculate_hash();

        Ok(Self {
            hash,
            inner: transaction,
            subtype: OnceLock::new(),
        })
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        // Do not try to decode untyped transactions
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Transaction for SystemTxEnvelope {
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
    use alloy::primitives::{B256, Uint};
    use zksync_os_contract_interface::InteropRoot;

    use crate::SystemTxEnvelope;

    /// System transaction serialization should be consistent with Ethereum JSON-RPC spec
    /// See https://ethereum.github.io/execution-apis/api-documentation/
    #[test]
    fn interop_roots_tx_serialization() {
        let tx = SystemTxEnvelope::import_interop_roots(vec![InteropRoot {
            chainId: Uint::from(1),
            blockOrBatchNumber: Uint::from(1),
            sides: vec![B256::ZERO],
        }]);

        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0x1f7117fa6190a6da113e9b7223222d3bc3b7c4c866772385e05ec79041e8f0ba",
  "initiator": "0x0000000000000000000000000000000000008001",
  "to": "0x0000000000000000000000000000000000010008",
  "gas": "0x0",
  "maxFeePerGas": "0x0",
  "maxPriorityFeePerGas": "0x0",
  "nonce": "0x0",
  "value": "0x0",
  "input": "0xcca2f7bc00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000",
  "v": "0x0",
  "r": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "s": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "yParity": "0x0"
}"#
        );
    }

    #[test]
    fn set_sl_chain_id_tx_serialization() {
        let tx = SystemTxEnvelope::set_sl_chain_id(1);

        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0x0db54bf16b232c227e16f783ea14f030ab983c67b5a2898452bc09028e0e5a4f",
  "initiator": "0x0000000000000000000000000000000000008001",
  "to": "0x000000000000000000000000000000000000800b",
  "gas": "0x0",
  "maxFeePerGas": "0x0",
  "maxPriorityFeePerGas": "0x0",
  "nonce": "0x0",
  "value": "0x0",
  "input": "0x040203e60000000000000000000000000000000000000000000000000000000000000001",
  "v": "0x0",
  "r": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "s": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "yParity": "0x0"
}"#
        );
    }
}
