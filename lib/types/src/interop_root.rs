use alloy::primitives::{B256, U256};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;


#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InteropRoot {
    pub chain_id: U256,
    pub block_number: U256,
    pub log_id: U256,
    pub sides: Vec<B256>,
}

impl TryFrom<NewInteropRoot> for InteropRoot {
    type Error = InteropRootError;

    fn try_from(interop_root_event: NewInteropRoot) -> Result<Self, Self::Error> {
        Ok(
            Self {
                chain_id: interop_root_event.chainId,
                block_number: interop_root_event.blockNumber,
                log_id: interop_root_event.logId,
                sides: interop_root_event.sides
            }
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InteropRootError {
    #[error("invalid interop root")]
    InteropError
}
