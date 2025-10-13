//! We need to not accidentally change the batch verification wire format
//! but there is no way in Rust to get a stable unique ID for a type,
//! so instead we define it in this separate file.
//!
//! Do not change this file under any circumstances. Copy it instead. May be deleted when obsolete.
//! (This is enforced by CI)

use bincode::{Decode, Encode};

/// The format BatchVerificationRequest is currently sent in
#[derive(Encode, Decode)]
pub struct BatchVerificationRequestWireFormatV1 {
    pub batch_number: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub request_id: u64,
    pub commit_data: Vec<u8>,
}

#[derive(Encode, Decode)]
pub enum BatchVerificationResponseResultWireFormatV1 {
    Success([u8; 65]),
    Refused(String),
}

/// The format BatchVerificationResponse is currently sent in
#[derive(Encode, Decode)]
pub struct BatchVerificationResponseWireFormatV1 {
    pub request_id: u64,
    pub result: BatchVerificationResponseResultWireFormatV1,
}
