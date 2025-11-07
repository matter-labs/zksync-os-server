//! We need to not accidentally change the replay wire format.
//!
//! Do not change this file under any circumstances. Copy it instead. May be deleted when obsolete.
//! (This is enforced by CI)

use bincode::{Decode, Encode};

// It is somewhat safe to assume that these will not change
use alloy::primitives::{Address, B256, U256};

// Differences from v3:
// - added `blob_fee`

/// The format ReplayRecords are currently sent in
#[derive(Encode, Decode)]
pub struct ReplayWireFormatV4 {
    pub block_context: BlockContext,
    pub starting_l1_priority_id: u64,
    pub transactions: Vec<ZkTransactionWireFormat>,
    pub previous_block_timestamp: u64,
    #[bincode(with_serde)]
    pub node_version: semver::Version,
    #[bincode(with_serde)]
    pub block_output_hash: B256,
}

#[derive(Encode, Decode)]
pub struct BlockContext {
    pub chain_id: u64,
    pub block_number: u64,
    #[bincode(with_serde)]
    pub block_hashes: BlockHashes,
    pub timestamp: u64,
    #[bincode(with_serde)]
    pub eip1559_basefee: U256,
    #[bincode(with_serde)]
    pub pubdata_price: U256,
    #[bincode(with_serde)]
    pub native_price: U256,
    #[bincode(with_serde)]
    pub coinbase: Address,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    #[bincode(with_serde)]
    pub mix_hash: U256,
    pub execution_version: u32,
    #[bincode(with_serde)]
    pub blob_fee: U256,
}

pub struct BlockHashes(pub [U256; 256]);

impl serde::Serialize for BlockHashes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.to_vec().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for BlockHashes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec: Vec<U256> = Vec::deserialize(deserializer)?;
        let array: [U256; 256] = vec
            .try_into()
            .map_err(|_| serde::de::Error::custom("Expected array of length 256"))?;
        Ok(Self(array))
    }
}

/// The transaction but EIP-2718 encoded.
/// Converting to a deep copy of the alloy types is way too much work to be worth it.
#[derive(Encode, Decode)]
pub struct ZkTransactionWireFormat(pub Vec<u8>);
