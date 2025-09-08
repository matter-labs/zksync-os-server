use crate::ReplayRecord;

mod conversion;

// Don't change the file even if we update formatting rules
#[rustfmt::skip]
mod v1;
#[rustfmt::skip]
mod v2;

pub const REPLAY_WIRE_FORMAT_VERSION: u32 = 2;

impl ReplayRecord {
    /// Encodes the replay using the current wire format version
    pub fn encode_with_current_version(self) -> Vec<u8> {
        let wire_format = v2::ReplayWireFormatV2::from(self);
        bincode::encode_to_vec(wire_format, bincode::config::standard()).unwrap()
    }

    /// Decodes the replay from the given bytes using the specified wire format version.
    /// Panics if the wire format version is too old.
    pub fn decode(bytes: &[u8], version: u32) -> Self {
        match version {
            1 => {
                let wire_format: v1::ReplayWireFormatV1 =
                    bincode::decode_from_slice(bytes, bincode::config::standard())
                        .unwrap()
                        .0;
                wire_format.into()
            }
            2 => {
                let wire_format: v2::ReplayWireFormatV2 =
                    bincode::decode_from_slice(bytes, bincode::config::standard())
                        .unwrap()
                        .0;
                wire_format.into()
            }
            _ => panic!("Unsupported replay wire format version: {version}"),
        }
    }
}
