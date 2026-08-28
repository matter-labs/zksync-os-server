use crate::ProtocolSemanticVersion;
use serde::{Deserialize, Serialize};

/// The DA *mechanism* a chain publishes its pubdata with.
///
/// This is one half of the chain's DA configuration and is fixed by the `L2DACommitmentScheme`
/// recorded on L1; the other half is [`crate::PubdataContent`], which says how *much* pubdata the
/// chain produces. The two are orthogonal: a logs-only validium keeps [`PubdataMode::Blobs`] — it
/// reaches L1 exactly like a rollup — and only narrows its pubdata content. Neither of them is a
/// pricing decision: pubdata *pricing* is a free fee-policy choice made on L1.
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

    /// The pubdata mode that produces `scheme`, i.e. the inverse of [`Self::da_commitment_scheme`].
    ///
    /// `None` for the schemes no ZKsync OS server can produce: `PubdataKeccak256` (third-party DA)
    /// and the `None` placeholder.
    pub fn from_da_commitment_scheme(
        scheme: zksync_os_contract_interface::models::DACommitmentScheme,
    ) -> Option<Self> {
        use zksync_os_contract_interface::models::DACommitmentScheme;
        match scheme {
            DACommitmentScheme::BlobsZKsyncOS => Some(Self::Blobs),
            DACommitmentScheme::BlobsAndPubdataKeccak256 => Some(Self::Calldata),
            DACommitmentScheme::EmptyNoDA => Some(Self::Validium),
            DACommitmentScheme::PubdataKeccak256 | DACommitmentScheme::None => None,
        }
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

#[cfg(test)]
mod tests {
    use super::PubdataMode;
    use zksync_os_contract_interface::models::DACommitmentScheme;

    #[test]
    fn every_pubdata_mode_round_trips_through_its_da_commitment_scheme() {
        for mode in [
            PubdataMode::Blobs,
            PubdataMode::Calldata,
            PubdataMode::Validium,
        ] {
            assert_eq!(
                PubdataMode::from_da_commitment_scheme(mode.da_commitment_scheme()),
                Some(mode),
                "{mode:?} does not round-trip through its DA commitment scheme"
            );
        }
    }

    #[test]
    fn schemes_no_server_produces_have_no_pubdata_mode() {
        // Third-party DA is not something a ZKsync OS server publishes itself, so a chain using it
        // has to configure its pubdata mode instead of deriving one.
        assert_eq!(
            PubdataMode::from_da_commitment_scheme(DACommitmentScheme::PubdataKeccak256),
            None
        );
        assert_eq!(
            PubdataMode::from_da_commitment_scheme(DACommitmentScheme::None),
            None
        );
    }
}
