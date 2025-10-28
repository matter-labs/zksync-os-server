use crate::{BatchVerificationRequest, BatchVerificationResponse};

mod conversion;

// Don't change the file even if we update formatting rules
#[rustfmt::skip]
mod v1;

#[cfg(test)]
mod tests;

pub const BATCH_VERIFICATION_WIRE_FORMAT_VERSION: u32 = 1;

impl BatchVerificationRequest {
    /// Encodes the request using the current wire format version
    pub fn encode_with_current_version(self) -> Vec<u8> {
        let wire_format = v1::BatchVerificationRequestWireFormatV1::from(self);
        bincode::encode_to_vec(wire_format, bincode::config::standard()).unwrap()
    }

    /// Decodes the request from the given bytes using the specified wire format version.
    /// Panics if the wire format version is too old.
    pub fn decode(bytes: &[u8], version: u32) -> Self {
        match version {
            1 => {
                let wire_format: v1::BatchVerificationRequestWireFormatV1 =
                    bincode::decode_from_slice(bytes, bincode::config::standard())
                        .unwrap()
                        .0;
                wire_format.into()
            }
            _ => panic!("Unsupported batch verification wire format version: {version}"),
        }
    }
}

impl BatchVerificationResponse {
    pub fn encode_with_version(self, version: u32) -> Vec<u8> {
        match version {
            1 => {
                let wire_format = v1::BatchVerificationResponseWireFormatV1::from(self);
                bincode::encode_to_vec(wire_format, bincode::config::standard()).unwrap()
            }
            _ => panic!("Unsupported batch verification wire format version: {version}"),
        }
    }

    /// Decodes the response from the given bytes using the specified wire format version.
    /// Panics if the wire format version is too old.
    pub fn decode(bytes: &[u8], version: u32) -> Result<Self, anyhow::Error> {
        match version {
            1 => {
                let wire_format: v1::BatchVerificationResponseWireFormatV1 =
                    bincode::decode_from_slice(bytes, bincode::config::standard())?.0;
                Ok(wire_format.try_into()?)
            }
            _ => panic!("Unsupported batch verification wire format version: {version}"),
        }
    }
}
