//! We need to not accidentally change the replay wire format
//! but there is no way in Rust to get a stable unique ID for a type,
//! so instead we define it in this separate file.
//!
//! Do not change this file under any circumstances. Copy it instead. May be deleted when obsolete.
//! (This is enforced by CI)

use bincode::{Decode, Encode};

// It is somewhat safe to assume that these will not change
use alloy::consensus::Signed;
use alloy::primitives::{Address, B256, U256, ruint::aliases::B160};

/// The format ReplayRecords are currently sent in
#[derive(Encode, Decode)]
pub struct ReplayWireFormat {
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

// A deep copy of ZkTransaction follows

// I don't believe that the EIP-specific data structures will change in future versions
use alloy::{
    consensus::{TxEip1559, TxEip2930, TxEip4844Variant, TxEip7702, TxLegacy},
    eips::eip7594::BlobTransactionSidecarVariant,
};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
// These are checked to be equal via TypeHash
use zksync_os_types::{L1PriorityEnvelope, L1UpgradeEnvelope};

#[derive(Encode, Decode)]
pub struct ZkTransactionWireFormat {
    #[bincode(with_serde)]
    pub signer: Address,
    #[bincode(with_serde)]
    pub inner: ZkEnvelope,
}

#[derive(Serialize, Deserialize)]
pub enum ZkEnvelope {
    Upgrade(L1UpgradeEnvelope),
    L1(L1PriorityEnvelope),
    L2(L2Envelope),
}

type L2Envelope = EthereumTxEnvelope<TxEip4844Variant<BlobTransactionSidecarVariant>>;

// Copying EthereumTxEnvelope is very painful but necessary because it will change with new EIPs.

#[derive(Clone)]
pub enum EthereumTxEnvelope<Eip4844: Serialize + DeserializeOwned + Clone + 'static> {
    Legacy(Signed<TxLegacy>),
    Eip2930(Signed<TxEip2930>),
    Eip1559(Signed<TxEip1559>),
    Eip4844(Signed<Eip4844>),
    Eip7702(Signed<TxEip7702>),
}
impl<Eip4844> Serialize for EthereumTxEnvelope<Eip4844>
where
    Eip4844: Serialize + DeserializeOwned + Clone + 'static,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let non_compat: alloy::consensus::EthereumTxEnvelope<Eip4844> = self.clone().into();
        let compat: alloy::consensus::serde_bincode_compat::EthereumTxEnvelope<Eip4844> =
            (&non_compat).into();
        compat.serialize(serializer)
    }
}

impl<'de, Eip4844> Deserialize<'de> for EthereumTxEnvelope<Eip4844>
where
    Eip4844: Serialize + DeserializeOwned + Clone + 'static,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let compat =
            alloy::consensus::serde_bincode_compat::EthereumTxEnvelope::<Eip4844>::deserialize(
                deserializer,
            )?;
        let non_compat: alloy::consensus::EthereumTxEnvelope<Eip4844> = compat.into();
        Ok(Self::from(non_compat))
    }
}

impl<Eip4844> From<EthereumTxEnvelope<Eip4844>> for alloy::consensus::EthereumTxEnvelope<Eip4844>
where
    Eip4844: Serialize + DeserializeOwned + Clone + 'static,
{
    fn from(value: EthereumTxEnvelope<Eip4844>) -> Self {
        match value {
            EthereumTxEnvelope::Legacy(tx) => Self::Legacy(tx),
            EthereumTxEnvelope::Eip2930(tx) => Self::Eip2930(tx),
            EthereumTxEnvelope::Eip1559(tx) => Self::Eip1559(tx),
            EthereumTxEnvelope::Eip4844(tx) => Self::Eip4844(tx),
            EthereumTxEnvelope::Eip7702(tx) => Self::Eip7702(tx),
        }
    }
}

impl<Eip4844> From<alloy::consensus::EthereumTxEnvelope<Eip4844>> for EthereumTxEnvelope<Eip4844>
where
    Eip4844: Serialize + DeserializeOwned + Clone + 'static,
{
    fn from(value: alloy::consensus::EthereumTxEnvelope<Eip4844>) -> Self {
        match value {
            alloy::consensus::EthereumTxEnvelope::Legacy(tx) => Self::Legacy(tx),
            alloy::consensus::EthereumTxEnvelope::Eip2930(tx) => Self::Eip2930(tx),
            alloy::consensus::EthereumTxEnvelope::Eip1559(tx) => Self::Eip1559(tx),
            alloy::consensus::EthereumTxEnvelope::Eip4844(tx) => Self::Eip4844(tx),
            alloy::consensus::EthereumTxEnvelope::Eip7702(tx) => Self::Eip7702(tx),
        }
    }
}
