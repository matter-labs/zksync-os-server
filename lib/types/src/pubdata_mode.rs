use crate::ProtocolSemanticVersion;
use serde::{Deserialize, Serialize};

/// The chain pubdata mode.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PubdataMode {
    Blobs = 0,
    Calldata = 1,
    Validium = 2,
}

impl PubdataMode {
    ///
    /// This method needed only during v29 => v30 protocol upgrade to ensure automatic pubdata mode change.
    ///
    /// Before v30 we didn't support blobs, and for some chains we want to automatically change pubdata mode from calldata to blobs during v30 upgrade.
    /// For this we set blobs DA in the config, but before the v30 upgrade it should be interpreted as calldata DA.
    ///
    pub fn adapt_for_protocol_version(&self, protocol_version: &ProtocolSemanticVersion) -> Self {
        if protocol_version.minor != 29 {
            return *self;
        }
        match self {
            Self::Blobs => Self::Calldata,
            Self::Calldata => Self::Calldata,
            Self::Validium => Self::Validium,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(PubdataMode::Blobs),
            1 => Some(PubdataMode::Calldata),
            2 => Some(PubdataMode::Validium),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn da_commitment_scheme(&self) -> zksync_os_contract_interface::models::DACommitmentScheme {
        match self {
            Self::Blobs => zksync_os_contract_interface::models::DACommitmentScheme::BlobsZKsyncOS,
            Self::Calldata => {
                zksync_os_contract_interface::models::DACommitmentScheme::BlobsAndPubdataKeccak256
            }
            Self::Validium => zksync_os_contract_interface::models::DACommitmentScheme::EmptyNoDA,
        }
    }
}
