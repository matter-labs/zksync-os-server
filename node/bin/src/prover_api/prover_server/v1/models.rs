use serde::{Deserialize, Serialize};
use zksync_os_types::proving_registry;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BatchDataPayload {
    pub batch_number: u64,
    pub vk_hash: String,
    pub prover_input: String, // base64‑encoded little‑endian u32 array
}

#[derive(Debug, Deserialize)]
pub(super) struct ProverQuery {
    pub id: String,
    /// Comma-separated vk_hashes of the proving stacks this prover supports.
    #[serde(default)]
    pub supported_vk_hashes: Option<String>,
}

impl ProverQuery {
    /// Verification-key hashes this prover declared support for.
    ///
    /// `None` means no declaration and the caller must not filter jobs. This is a
    /// backwards-compatibility layer: old provers don't send `supported_vk_hashes`
    /// and must keep receiving jobs as before.
    ///
    /// Provers currently declare exactly one stack; the list shape is for
    /// multi-stack provers, which become possible on the prover side with
    /// airbender v2.
    ///
    /// A declared hash the server doesn't recognize is skipped with a warning: it
    /// most likely belongs to a proving stack newer than this server, and the prover should
    /// still be served the stacks both sides know. If nothing in the declaration
    /// is recognized, this returns `Some(vec![])` so that such a prover gets *no*
    /// jobs rather than any jobs.
    pub fn supported_vk_hashes(&self) -> Option<Vec<&'static str>> {
        let hashes: Vec<&str> = self
            .supported_vk_hashes
            .iter()
            .flat_map(|hashes| hashes.split(','))
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .collect();

        if hashes.is_empty() {
            return None;
        }

        let hashes = hashes
            .into_iter()
            .filter_map(|hash| {
                if let Some(canonical_hash) =
                    proving_registry().canonical_verification_key_hash(hash)
                {
                    Some(canonical_hash)
                } else {
                    tracing::warn!(
                        prover_id = self.id,
                        vk_hash = hash,
                        "prover declared a vk_hash unknown to this server; ignoring it"
                    );
                    None
                }
            })
            .collect();

        Some(hashes)
    }
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
pub(super) struct FailedProofResponse {
    pub batch_number: u64,
    pub last_batch_timestamp: u64,
    pub expected_hash_u32s: [u32; 8],
    pub proof_final_register_values: [u32; 16],
    pub vk_hash: String,
    pub proof: String, // base64‑encoded FRI proof (little‑endian u32 array)
}

#[cfg(test)]
mod tests {
    use super::ProverQuery;
    use zksync_os_types::proving_registry;

    const UNKNOWN_VK_HASH: &str =
        "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    fn query(supported_vk_hashes: Option<&str>) -> ProverQuery {
        ProverQuery {
            id: "test_prover".to_string(),
            supported_vk_hashes: supported_vk_hashes.map(str::to_string),
        }
    }

    fn known_vk_hashes() -> Vec<&'static str> {
        let mut hashes: Vec<_> = proving_registry()
            .iter()
            .map(|entry| entry.configuration.verification_key_hash)
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        hashes
    }

    #[test]
    fn no_declaration_means_no_filter() {
        assert_eq!(query(None).supported_vk_hashes(), None);
        // Declared-but-blank is treated the same as absent
        assert_eq!(query(Some("")).supported_vk_hashes(), None);
        assert_eq!(query(Some(" ,, ")).supported_vk_hashes(), None);
    }

    #[test]
    fn known_hashes_are_parsed() {
        let known = known_vk_hashes();
        let q = query(Some(&known.join(", ")));
        assert_eq!(q.supported_vk_hashes(), Some(known));
    }

    #[test]
    fn unknown_hash_is_skipped_keeping_known_ones() {
        let known = known_vk_hashes()[0];
        let q = query(Some(&format!("{UNKNOWN_VK_HASH},{known}")));
        assert_eq!(q.supported_vk_hashes(), Some(vec![known]));
    }

    #[test]
    fn all_unknown_hashes_mean_no_jobs_not_no_filter() {
        let q = query(Some(UNKNOWN_VK_HASH));
        assert_eq!(q.supported_vk_hashes(), Some(vec![]));
    }
}
