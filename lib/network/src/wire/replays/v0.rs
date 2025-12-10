//! Fake version used only for testing.

use alloy::primitives::BlockNumber;
use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub block_number: BlockNumber,
}
