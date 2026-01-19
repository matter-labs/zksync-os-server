use std::marker::PhantomData;

use crate::transaction::{system::envelope::SystemTransactionEnvelope, tx::SystemTransaction};
use alloy::primitives::{Address, Bytes, address};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::{IMessageRoot::addInteropRootsInBatchCall, InteropRoot};

pub mod envelope;
pub mod tx;

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");

// todo: Check if this value is good enough
const DEFAULT_GAS_LIMIT: u64 = 72_000_000;

pub type InteropRootsEnvelope = SystemTransactionEnvelope<InteropRootsTxType>;

impl InteropRootsEnvelope {
    pub fn from_interop_roots(interop_roots: Vec<InteropRoot>) -> Self {
        let calldata = addInteropRootsInBatchCall {
            interopRootsInput: interop_roots,
        }
        .abi_encode();

        let transaction = SystemTransaction {
            gas_limit: DEFAULT_GAS_LIMIT,
            to: L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS,
            input: Bytes::from(calldata),
            marker: PhantomData,
        };

        Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
        }
    }

    pub fn interop_roots_count(&self) -> u64 {
        let interop_roots = addInteropRootsInBatchCall::abi_decode(&self.inner.input)
            .expect("Failed to decode interop roots calldata")
            .interopRootsInput;
        interop_roots.len() as u64
    }
}

pub trait SystemTxType: Clone + Send + Sync + std::fmt::Debug + 'static {
    const TX_TYPE: u8;
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct InteropRootsTxType;

impl SystemTxType for InteropRootsTxType {
    const TX_TYPE: u8 = 0x7d;
}

#[cfg(test)]
mod tests {
    use crate::InteropRootsEnvelope;
    use crate::transaction::tx::SystemTransaction;

    #[test]
    fn interop_roots_tx_serialization() {
        // Interop roots serialization should be consistent with Ethereum JSON-RPC spec
        // See https://ethereum.github.io/execution-apis/api-documentation/

        let transaction = SystemTransaction {
            gas_limit: 0x10000,
            to: Default::default(),
            input: Default::default(),
            marker: Default::default(),
        };

        let tx = InteropRootsEnvelope {
            hash: transaction.calculate_hash(),
            inner: transaction,
        };

        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0xd06b6df7ff36db8daee83e3a8d5d0b1e349e57968054b3da83192341e195a848",
  "initiator": "0x0000000000000000000000000000000000008001",
  "to": "0x0000000000000000000000000000000000000000",
  "gas": "0x10000",
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
