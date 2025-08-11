use alloy::consensus::{Block, Sealed};
use alloy::primitives::B256;
use zksync_os_storage_api::RepositoryBlock;

pub fn build_genesis() -> RepositoryBlock {
    // todo: build genesis from config
    Sealed::new_unchecked(Block::default(), B256::ZERO)
}
