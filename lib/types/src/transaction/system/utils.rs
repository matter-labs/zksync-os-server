use alloy::{
    primitives::{Address, Bytes, ChainId, U256, address},
    sol_types::SolCall,
};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::{
    IMessageRoot::addInteropRootsInBatchCall, ISystemContext::setSettlementLayerChainIdCall,
    InteropRoot,
};

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");
pub const SYSTEM_CONTEXT_ADDRESS: Address = address!("0x000000000000000000000000000000000000800b");

pub const SYSTEM_TX_TYPE_ID: u8 = 125;

/// Enum to represent the subtype of system transaction
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub enum SystemTxType {
    /// The transaction type for importing interop roots, contains the number of interop roots imported
    ImportInteropRoots(u64),
    /// The transaction type for setting the settlement layer chain id
    SetSLChainId,
}

/// Helper type to encode/decode system transaction input and determine it's subtype
pub(crate) enum SystemTxInput {
    ImportInteropRoots(Vec<InteropRoot>),
    SetSLChainId(ChainId),
}

impl SystemTxInput {
    pub fn abi_encode(&self) -> Vec<u8> {
        match self {
            Self::ImportInteropRoots(roots) => addInteropRootsInBatchCall {
                interopRootsInput: roots.clone(),
            }
            .abi_encode(),
            Self::SetSLChainId(chain_id) => setSettlementLayerChainIdCall {
                _newSettlementLayerChainId: U256::from(*chain_id),
            }
            .abi_encode(),
        }
    }

    pub fn to_address(&self) -> Address {
        match self {
            Self::ImportInteropRoots(_) => L2_INTEROP_ROOT_STORAGE_ADDRESS,
            Self::SetSLChainId(_) => SYSTEM_CONTEXT_ADDRESS,
        }
    }

    pub fn abi_decode(data: &Bytes) -> Self {
        match addInteropRootsInBatchCall::abi_decode(data) {
            Ok(call) => Self::ImportInteropRoots(call.interopRootsInput),
            Err(_) => {
                let call = setSettlementLayerChainIdCall::abi_decode(data)
                    .expect("failed to decode system transaction");
                Self::SetSLChainId(call._newSettlementLayerChainId.try_into().unwrap())
            }
        }
    }
}
