use alloy::consensus::{Header, Sealed};
use alloy::primitives::B256;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::{AccountDiff, StorageWrite, TxOutput};

use crate::ExecutionVersion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockPubdata {
    Bytes(Vec<u8>),
    Length(u64),
}

impl BlockPubdata {
    pub fn used(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::Length(length) => *length,
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Length(_) => None,
        }
    }

    pub fn expect_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Length(length) => {
                panic!("expected block pubdata bytes, found length-only pubdata: {length}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockOutput {
    pub header: Sealed<Header>,
    pub tx_results: Vec<Result<TxOutput, InvalidTransaction>>,
    pub storage_writes: Vec<StorageWrite>,
    pub account_diffs: Vec<AccountDiff>,
    pub published_preimages: Vec<(B256, Vec<u8>)>,
    pub pubdata: BlockPubdata,
    pub computational_native_used: u64,
}

impl BlockOutput {
    pub fn pubdata_used(&self) -> u64 {
        self.pubdata.used()
    }

    pub fn pubdata_bytes(&self) -> Option<&[u8]> {
        self.pubdata.bytes()
    }

    pub fn expect_pubdata_bytes(&self) -> &[u8] {
        self.pubdata.expect_bytes()
    }

    pub fn assert_pubdata_form_for_execution(&self, execution_version: ExecutionVersion) {
        match execution_version {
            ExecutionVersion::V1
            | ExecutionVersion::V2
            | ExecutionVersion::V3
            | ExecutionVersion::V4
            | ExecutionVersion::V5
            | ExecutionVersion::V6 => {
                assert!(
                    matches!(&self.pubdata, BlockPubdata::Bytes(..)),
                    "execution version {execution_version:?} must emit full pubdata bytes",
                );
            }
            ExecutionVersion::V7 => {
                assert!(
                    matches!(&self.pubdata, BlockPubdata::Length(..)),
                    "execution version {execution_version:?} must emit length-only pubdata",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockOutput, BlockPubdata};
    use crate::ExecutionVersion;
    use alloy::consensus::{Header, Sealable};

    fn block_output(pubdata: BlockPubdata) -> BlockOutput {
        BlockOutput {
            header: Header::default().seal_slow(),
            tx_results: vec![],
            storage_writes: vec![],
            account_diffs: vec![],
            published_preimages: vec![],
            pubdata,
            computational_native_used: 0,
        }
    }

    #[test]
    fn bytes_pubdata_reports_its_length() {
        let output = block_output(BlockPubdata::Bytes(vec![1, 2, 3]));
        assert_eq!(output.pubdata_used(), 3);
        assert_eq!(output.pubdata_bytes(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn length_only_pubdata_reports_usage_without_bytes() {
        let output = block_output(BlockPubdata::Length(7));
        assert_eq!(output.pubdata_used(), 7);
        assert_eq!(output.pubdata_bytes(), None);
    }

    #[test]
    #[should_panic(expected = "must emit length-only pubdata")]
    fn execution_v7_rejects_full_pubdata_bytes() {
        let output = block_output(BlockPubdata::Bytes(vec![0]));
        output.assert_pubdata_form_for_execution(ExecutionVersion::V7);
    }
}
