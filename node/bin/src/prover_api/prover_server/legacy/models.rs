use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BatchDataPayload {
    pub block_number: u64,
    pub prover_input: String, // base64‑encoded little‑endian u32 array
}

#[derive(Debug, Deserialize)]
pub(super) struct ProverQuery {
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FriProofPayload {
    pub block_number: u64,
    pub proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct NextSnarkProverJobPayload {
    pub block_number_from: u64,
    pub block_number_to: u64,
    pub fri_proofs: Vec<String>, // base64‑encoded FRI proofs (little‑endian u32 array)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SnarkProofPayload {
    pub block_number_from: u64,
    pub block_number_to: u64,
    pub proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AvailableProofsPayload {
    block_number: u64,
    available_proofs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FailedProofResponse {
    pub batch_number: u64,
    pub last_block_timestamp: u64,
    pub expected_hash_u32s: [u32; 8],
    pub proof_final_register_values: [u32; 16],
    pub proof: String, // base64‑encoded FRI proof (little‑endian u32 array)
}
