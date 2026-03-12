pub mod l1;
pub mod l2;

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;

/// Parameters for the storage proof verification pipeline.
pub struct VerifyParams {
    pub address: Address,
    pub keys: Vec<B256>,
    pub batch_number: u64,
    pub l1_contract: Option<Address>,
    pub bridgehub: Option<Address>,
}

/// Result of a successful storage proof verification.
#[derive(Debug)]
pub struct VerificationResult {
    /// The state commitment derived from the proof's Merkle tree root + metadata.
    pub storage_commitment: B256,
    /// The batch hash fetched from the L1 `BlockCommit` event.
    pub l1_batch_hash: B256,
    /// Proven storage values, in the order of the queried keys.
    /// `None` means the slot does not exist in the tree.
    pub storage_values: Vec<(B256, Option<B256>)>,
}

/// Runs the full verification pipeline:
/// 1. Fetches the storage proof from L2 via `zks_getProof`
/// 2. Resolves the diamond proxy address (auto-discovery or override)
/// 3. Fetches the `batchHash` from the L1 `BlockCommit` event
/// 4. Verifies the Merkle proof (Blake2s tree + state commitment preimage)
/// 5. Compares the computed commitment against L1
/// 6. Returns proven storage values
pub async fn verify_storage_proof(
    l1_provider: &(impl Provider + Clone),
    l2_provider: &impl Provider,
    params: VerifyParams,
) -> anyhow::Result<VerificationResult> {
    // 1. Fetch proof from L2
    let proof = l2::fetch_proof(
        l2_provider,
        params.address,
        params.keys.clone(),
        params.batch_number,
    )
    .await?;

    // 2. Resolve diamond proxy
    let diamond_proxy = l1::resolve_diamond_proxy(
        l1_provider,
        l2_provider,
        params.l1_contract,
        params.bridgehub,
    )
    .await?;

    // 3. Fetch batchHash from L1
    let l1_batch_hash =
        l1::fetch_l1_batch_hash(l1_provider, diamond_proxy, params.batch_number).await?;

    // 4. Verify the proof internally (Merkle tree + state commitment preimage)
    let view = proof.verify(params.address, &params.keys)?;

    // 5. Compare commitments
    anyhow::ensure!(
        view.storage_commitment == l1_batch_hash,
        "Storage commitment mismatch!\n  Proof:  {}\n  L1:     {}",
        view.storage_commitment,
        l1_batch_hash,
    );

    // 6. Build result
    let storage_values = params
        .keys
        .iter()
        .zip(view.storage_values.iter())
        .map(|(key, value)| (*key, *value))
        .collect();

    Ok(VerificationResult {
        storage_commitment: view.storage_commitment,
        l1_batch_hash,
        storage_values,
    })
}
