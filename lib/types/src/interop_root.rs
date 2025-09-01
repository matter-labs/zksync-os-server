use alloy::primitives::B256;
use alloy::rpc::types::Log;
use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zk_ee::system::metadata::InteropRoot as ZkOsInteropRoot;
use zk_ee::utils::Bytes32;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Encode, Decode)]
pub struct InteropRootPosition {
    pub sl_block_number: u64,
    pub log_index_in_block: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InteropRoot {
    pub chain_id: u64,
    pub sides: Vec<B256>,
    pub pos: InteropRootPosition,
}

impl From<(NewInteropRoot, Log)> for InteropRoot {
    fn from((interop_root_event, log): (NewInteropRoot, Log)) -> Self {
        Self {
            chain_id: interop_root_event.chainId.to::<u64>(),
            sides: interop_root_event.sides,
            pos: InteropRootPosition {
                sl_block_number: interop_root_event.blockNumber.to::<u64>(),
                log_index_in_block: log.log_index.unwrap(),
            }
        }
    }
}

impl From<InteropRoot> for ZkOsInteropRoot {
    fn from(value: InteropRoot) -> Self {
        ZkOsInteropRoot {
            chain_id: value.chain_id,
            root: Bytes32::from_array(value.sides[0].0),
            block_or_batch_number: value.pos.sl_block_number,
        }
    }
}
