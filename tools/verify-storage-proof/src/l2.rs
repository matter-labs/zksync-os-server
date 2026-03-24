use alloy::network::Network;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use zksync_os_rpc_api::types::BatchStorageProof;

/// Fetches a storage proof from the L2 node via `zks_getProof`.
pub async fn fetch_proof<N: Network>(
    l2_provider: &impl Provider<N>,
    address: Address,
    keys: Vec<B256>,
    batch_number: u64,
) -> anyhow::Result<BatchStorageProof> {
    let proof: Option<BatchStorageProof> = l2_provider
        .client()
        .request("zks_getProof", (address, &keys, batch_number))
        .await?;
    proof.ok_or_else(|| anyhow::anyhow!("No proof returned for batch {batch_number}"))
}
