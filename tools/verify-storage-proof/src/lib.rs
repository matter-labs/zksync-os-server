pub mod l1;
pub mod l2;

use std::time::Duration;

use alloy::network::Network;
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolValue;

sol! {
    struct StoredBatchInfo {
        uint64 batchNumber;
        bytes32 batchHash;
        uint64 indexRepeatedStorageChanges;
        uint256 numberOfLayer1Txs;
        bytes32 priorityOperationsHash;
        bytes32 dependencyRootsRollingHash;
        bytes32 l2LogsTreeRoot;
        uint256 timestamp;
        bytes32 commitment;
    }
}

/// Parameters for the storage proof verification pipeline.
pub struct VerifyParams {
    pub address: Address,
    pub keys: Vec<B256>,
    pub batch_number: u64,
    pub l1_contract: Option<Address>,
    pub bridgehub: Option<Address>,
    /// If set, wait up to this duration for the batch to be committed on L1
    /// instead of failing immediately when `storedBatchHash` returns zero.
    pub commit_timeout: Option<Duration>,
}

/// Result of a successful storage proof verification.
#[derive(Debug)]
pub struct VerificationResult {
    /// The `keccak256(abi.encode(StoredBatchInfo))` reconstructed from the proof.
    pub computed_batch_hash: B256,
    /// The on-chain `storedBatchHash(batchNumber)` from the diamond proxy.
    pub on_chain_batch_hash: B256,
    /// Proven storage values, in the order of the queried keys.
    /// `None` means the slot does not exist in the tree.
    pub storage_values: Vec<(B256, Option<B256>)>,
}

/// Runs the full verification pipeline:
/// 1. Fetches the storage proof from L2 via `zks_getProof`
/// 2. Verifies the Merkle proof (Blake2s tree + state commitment preimage)
/// 3. Resolves the diamond proxy address (auto-discovery or override)
/// 4. Fetches `storedBatchHash(batchNumber)` from L1
/// 5. Reconstructs `StoredBatchInfo`, hashes it, and compares against L1
/// 6. Returns proven storage values
pub async fn verify_storage_proof<N: Network>(
    l1_provider: &impl Provider,
    l2_provider: &impl Provider<N>,
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

    // 2. Verify batch number matches
    let l1_verification_data = &proof.l1_verification_data;
    anyhow::ensure!(
        l1_verification_data.batch_number == params.batch_number,
        "Batch number mismatch: requested {}, proof contains {}",
        params.batch_number,
        l1_verification_data.batch_number,
    );

    // 3. Verify the proof internally (Merkle tree + state commitment preimage)
    let view = proof.verify(params.address, &params.keys)?;

    // 4. Resolve diamond proxy
    let diamond_proxy = l1::resolve_diamond_proxy(
        l1_provider,
        l2_provider,
        params.l1_contract,
        params.bridgehub,
    )
    .await?;

    // 5. Fetch on-chain batch hash (with optional wait)
    let on_chain_batch_hash = if let Some(timeout) = params.commit_timeout {
        wait_for_batch_hash(l1_provider, diamond_proxy, params.batch_number, timeout).await?
    } else {
        l1::fetch_stored_batch_hash(l1_provider, diamond_proxy, params.batch_number).await?
    };

    // 6. Reconstruct StoredBatchInfo from proof data + state commitment, hash, and compare
    let stored_batch_info = StoredBatchInfo {
        batchNumber: l1_verification_data.batch_number,
        batchHash: view.storage_commitment,
        indexRepeatedStorageChanges: 0,
        numberOfLayer1Txs: U256::from(l1_verification_data.number_of_layer1_txs),
        priorityOperationsHash: l1_verification_data.priority_operations_hash,
        dependencyRootsRollingHash: l1_verification_data.dependency_roots_rolling_hash,
        l2LogsTreeRoot: l1_verification_data.l2_to_l1_logs_root_hash,
        timestamp: U256::ZERO,
        commitment: l1_verification_data.commitment,
    };
    let computed_batch_hash = keccak256(stored_batch_info.abi_encode_params());

    anyhow::ensure!(
        computed_batch_hash == on_chain_batch_hash,
        "Batch hash mismatch!\n  Computed: {computed_batch_hash}\n  L1:       {on_chain_batch_hash}",
    );

    // 7. Build result
    let storage_values = params
        .keys
        .iter()
        .zip(view.storage_values.iter())
        .map(|(key, value)| (*key, *value))
        .collect();

    Ok(VerificationResult {
        computed_batch_hash,
        on_chain_batch_hash,
        storage_values,
    })
}

/// Polls `storedBatchHash` until it returns a non-zero value or the timeout expires.
async fn wait_for_batch_hash(
    provider: &impl Provider,
    diamond_proxy: Address,
    batch_number: u64,
    timeout: Duration,
) -> anyhow::Result<B256> {
    tokio::time::timeout(timeout, async {
        loop {
            match l1::fetch_stored_batch_hash(provider, diamond_proxy, batch_number).await {
                Ok(hash) => return Ok(hash),
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("Timed out waiting for batch {batch_number} to be committed on L1")
    })?
}
