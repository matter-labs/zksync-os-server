use alloy::network::Network;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol;
use anyhow::Context;

sol! {
    interface IBridgehub {
        function getZKChain(uint256 _chainId) external view returns (address);
    }

    interface IZKChain {
        function storedBatchHash(uint256 _batchNumber) external view returns (bytes32);
    }
}

/// Resolves the diamond proxy address. Uses the override if provided, otherwise
/// auto-discovers via bridgehub by fetching the chain ID from L2.
pub async fn resolve_diamond_proxy<N: Network>(
    l1_provider: &impl Provider,
    l2_provider: &impl Provider<N>,
    l1_contract_override: Option<Address>,
    bridgehub_override: Option<Address>,
) -> anyhow::Result<Address> {
    if let Some(addr) = l1_contract_override {
        return Ok(addr);
    }

    let bridgehub =
        bridgehub_override.context("Either --l1-contract or --bridgehub must be provided")?;

    discover_diamond_proxy(l1_provider, l2_provider, bridgehub).await
}

/// Fetches chain ID from L2, then calls `bridgehub.getZKChain(chainId)` on L1.
async fn discover_diamond_proxy<N: Network>(
    l1_provider: &impl Provider,
    l2_provider: &impl Provider<N>,
    bridgehub: Address,
) -> anyhow::Result<Address> {
    let chain_id = l2_provider.get_chain_id().await?;

    let call = IBridgehub::getZKChainCall {
        _chainId: U256::from(chain_id),
    };
    let result = l1_provider
        .call(
            alloy::rpc::types::TransactionRequest::default()
                .to(bridgehub)
                .input(
                    alloy::primitives::Bytes::from(
                        <IBridgehub::getZKChainCall as alloy::sol_types::SolCall>::abi_encode(
                            &call,
                        ),
                    )
                    .into(),
                ),
        )
        .await?;
    let diamond_proxy =
        <IBridgehub::getZKChainCall as alloy::sol_types::SolCall>::abi_decode_returns(&result)?;

    anyhow::ensure!(
        diamond_proxy != Address::ZERO,
        "Bridgehub returned zero address for chain ID {chain_id} — chain not registered"
    );

    Ok(diamond_proxy)
}

/// Calls `storedBatchHash(batchNumber)` on the diamond proxy contract to get the
/// on-chain batch hash (= `keccak256(abi.encode(StoredBatchInfo))`).
pub async fn fetch_stored_batch_hash(
    l1_provider: &impl Provider,
    diamond_proxy: Address,
    batch_number: u64,
) -> anyhow::Result<B256> {
    let call = IZKChain::storedBatchHashCall {
        _batchNumber: U256::from(batch_number),
    };
    let result = l1_provider
        .call(
            alloy::rpc::types::TransactionRequest::default()
                .to(diamond_proxy)
                .input(
                    alloy::primitives::Bytes::from(
                        <IZKChain::storedBatchHashCall as alloy::sol_types::SolCall>::abi_encode(
                            &call,
                        ),
                    )
                    .into(),
                ),
        )
        .await?;
    let hash =
        <IZKChain::storedBatchHashCall as alloy::sol_types::SolCall>::abi_decode_returns(&result)?;

    anyhow::ensure!(
        hash != B256::ZERO,
        "storedBatchHash returned zero for batch {batch_number} — batch not committed yet"
    );

    Ok(hash)
}
