//! We need to not accidentally change the replay wire format
//! but there is no way in Rust to get a stable unique ID for a type,
//! so instead we define it in this separate file.
//! Any change to this file necessitates a major version bump.

/// If you change this file, you must increment this number, unless
/// some other change after the most recent major version already bumped it.
pub const REPLAY_WIRE_FORMAT_VERSION: u32 = 1;

use bincode::{Decode, Encode};

// It is somewhat safe to assume that these will not change
use alloy::primitives::{B256, U256, ruint::aliases::B160};

// If ZkTransaction changes, we'll cut a major version anyway.
use zksync_os_types::ZkTransaction;

/// The format ReplayRecords are currently sent in
#[derive(Encode, Decode)]
pub struct ReplayWireFormat {
    pub block_context: BlockContext,
    pub starting_l1_priority_id: u64,
    pub transactions: Vec<ZkTransaction>,
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
    pub gas_per_pubdata: U256,
    #[bincode(with_serde)]
    pub native_price: U256,
    #[bincode(with_serde)]
    pub coinbase: B160,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    #[bincode(with_serde)]
    pub mix_hash: U256,
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

/// The format ReplayRecords were sent in at the previous major version.
#[derive(Encode, Decode)]
pub struct PreviousReplayWireFormat(pub ReplayWireFormat); // Currently just a dummy because there is no previous version.
