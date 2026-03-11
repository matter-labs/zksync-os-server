use alloy::{
    primitives::{Address, ChainId, U256, address},
    sol_types::SolCall,
};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::{
    IInteropCenter::setInteropFeeCall, IMessageRoot::addInteropRootsInBatchCall,
    ISystemContext::setSettlementLayerChainIdCall, InteropRoot,
};

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");
pub const L2_INTEROP_CENTER_ADDRESS: Address =
    address!("0x000000000000000000000000000000000001000d");
pub const SYSTEM_CONTEXT_ADDRESS: Address = address!("0x000000000000000000000000000000000000800b");

pub const SYSTEM_TX_TYPE_ID: u8 = 125;

/// Enum to represent the subtype of system transaction
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub enum SystemTxType {
    /// Transaction subtype for importing interop roots, contains the number of interop roots imported
    ImportInteropRoots(u64),
    /// Transaction subtype for setting the settlement layer chain id, contains migration number
    SetSLChainId(u64),
    /// Transaction subtype for setting the interop fee, contains interop fee update number.
    SetInteropFee(u64),
}

/// Helper type to encode/decode system transaction input and determine it's subtype
pub(crate) enum SystemTxInput {
    ImportInteropRoots(Vec<InteropRoot>),
    SetSLChainId(ChainId, u64),
    SetInteropFee(U256, u64),
}

impl SystemTxInput {
    pub fn encode_data(&self) -> (Vec<u8>, u64) {
        match self {
            Self::ImportInteropRoots(roots) => (
                addInteropRootsInBatchCall {
                    interopRootsInput: roots.clone(),
                }
                .abi_encode(),
                0,
            ),
            Self::SetSLChainId(chain_id, salt) => (
                setSettlementLayerChainIdCall {
                    _newSettlementLayerChainId: U256::from(*chain_id),
                }
                .abi_encode(),
                *salt,
            ),
            Self::SetInteropFee(interop_fee, salt) => (
                setInteropFeeCall {
                    _interopFee: *interop_fee,
                }
                .abi_encode(),
                *salt,
            ),
        }
    }

    pub fn to_address(&self) -> Address {
        match self {
            Self::ImportInteropRoots(_) => L2_INTEROP_ROOT_STORAGE_ADDRESS,
            Self::SetSLChainId(_, _) => SYSTEM_CONTEXT_ADDRESS,
            Self::SetInteropFee(_, _) => L2_INTEROP_CENTER_ADDRESS,
        }
    }
}
