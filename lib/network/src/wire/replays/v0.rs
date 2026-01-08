//! Fake version used only for testing.

use alloy::primitives::BlockNumber;
use alloy_rlp::{RlpDecodable, RlpEncodable};

#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct ReplayRecord {
    pub block_number: BlockNumber,
}
