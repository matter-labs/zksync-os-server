use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BatchDataPayload {
    pub batch_number: u64,
    pub vk_hash: String,
    pub prover_input: String, // base64‑encoded little‑endian u32 array
}

#[derive(Debug, Deserialize)]
pub(super) struct ProverQuery {
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FriProofPayload {
    pub batch_number: u64,
    pub vk_hash: String,
    pub proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct NextSnarkProverJobPayload {
    pub from_batch_number: u64,
    pub to_batch_number: u64,
    pub vk_hash: String,
    pub fri_proofs: Vec<String>, // base64‑encoded FRI proofs (little‑endian u32 array)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SnarkProofPayload {
    pub from_batch_number: u64,
    pub to_batch_number: u64,
    pub vk_hash: String,
    pub proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AvailableProofsPayload {
    batch_number: u64,
    available_proofs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FailedProofResponse {
    pub batch_number: u64,
    pub last_batch_timestamp: u64,
    pub expected_hash_u32s: [u32; 8],
    pub proof_final_register_values: [u32; 16],
    pub vk_hash: String,
    pub proof: String, // base64‑encoded FRI proof (little‑endian u32 array)
}
