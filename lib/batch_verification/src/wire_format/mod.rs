use crate::{BatchVerificationRequest, BatchVerificationResponse};

mod conversion;
mod payload;
pub(crate) use payload::BatchVerificationCommitInfo;

// Don't change files even if we update formatting rules
#[rustfmt::skip]
mod v1;
// NOTE: v2 is intentionally not wired into the active transport path yet.
// Reintroduce it deliberately when the protocol upgrade is ready and EN rollout
// can happen in lockstep with the wire-format bump.

#[cfg(test)]
mod tests;

pub const BATCH_VERIFICATION_WIRE_FORMAT_VERSION: u32 = 1;

pub(crate) fn ensure_supported_wire_format(version: u32) -> anyhow::Result<()> {
    if version == BATCH_VERIFICATION_WIRE_FORMAT_VERSION {
        Ok(())
    } else {
        anyhow::bail!("Unsupported batch verification wire format version: {version}")
    }
}

impl BatchVerificationRequest {
    pub fn encode(self) -> Vec<u8> {
        let wire_format = v1::BatchVerificationRequestWireFormatV1::from(self);
        bincode::encode_to_vec(wire_format, bincode::config::standard()).unwrap()
    }

    pub fn decode(bytes: &[u8]) -> Self {
        let wire_format: v1::BatchVerificationRequestWireFormatV1 =
            bincode::decode_from_slice(bytes, bincode::config::standard())
                .unwrap()
                .0;
        wire_format.into()
    }
}

impl BatchVerificationResponse {
    pub fn encode(self) -> Vec<u8> {
        let wire_format = v1::BatchVerificationResponseWireFormatV1::from(self);
        bincode::encode_to_vec(wire_format, bincode::config::standard()).unwrap()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        let wire_format: v1::BatchVerificationResponseWireFormatV1 =
            bincode::decode_from_slice(bytes, bincode::config::standard())?.0;
        wire_format.try_into()
    }
}
