use crate::IExecutor;
use alloy::primitives::B256;

/// User-friendly version of [`IExecutor::PriorityOpsBatchInfo`].
#[derive(Clone, Debug, Default)]
pub struct PriorityOpsBatchInfo {
    pub left_path: Vec<B256>,
    pub right_path: Vec<B256>,
    pub item_hashes: Vec<B256>,
}

impl From<PriorityOpsBatchInfo> for IExecutor::PriorityOpsBatchInfo {
    fn from(value: PriorityOpsBatchInfo) -> Self {
        IExecutor::PriorityOpsBatchInfo {
            leftPath: value.left_path,
            rightPath: value.right_path,
            itemHashes: value.item_hashes,
        }
    }
}
