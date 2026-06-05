use alloy::consensus::{Header, Sealed};
use alloy::primitives::B256;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::{AccountDiff, StorageWrite, TxOutput};

#[derive(Debug, Clone)]
pub struct BlockOutput {
    pub header: Sealed<Header>,
    pub tx_results: Vec<Result<TxOutput, InvalidTransaction>>,
    pub storage_writes: Vec<StorageWrite>,
    pub account_diffs: Vec<AccountDiff>,
    pub published_preimages: Vec<(B256, Vec<u8>)>,
    pub pubdata: Vec<u8>,
    pub computational_native_used: u64,
}
