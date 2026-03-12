use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    interface IBridgehub {
        function getZKChain(uint256 _chainId) external view returns (address);
    }

    event BlockCommit(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
}

/// Resolves the diamond proxy address. Uses the override if provided, otherwise
/// auto-discovers via bridgehub by fetching the chain ID from L2.
pub async fn resolve_diamond_proxy(
    l1_provider: &(impl Provider + Clone),
    l2_provider: &impl Provider,
    l1_contract_override: Option<Address>,
    bridgehub_override: Option<Address>,
) -> anyhow::Result<Address> {
    if let Some(addr) = l1_contract_override {
        return Ok(addr);
    }

    let bridgehub = bridgehub_override
        .ok_or_else(|| anyhow::anyhow!("Either --l1-contract or --bridgehub must be provided"))?;

    discover_diamond_proxy(l1_provider, l2_provider, bridgehub).await
}

/// Fetches chain ID from L2, then calls `bridgehub.getZKChain(chainId)` on L1.
async fn discover_diamond_proxy(
    l1_provider: &(impl Provider + Clone),
    l2_provider: &impl Provider,
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

/// Queries `BlockCommit` event logs for the given batch number and extracts the
/// `batchHash` (state commitment) from the event's second indexed topic.
pub async fn fetch_l1_batch_hash(
    l1_provider: &impl Provider,
    diamond_proxy: Address,
    batch_number: u64,
) -> anyhow::Result<B256> {
    let filter = Filter::new()
        .address(diamond_proxy)
        .event_signature(BlockCommit::SIGNATURE_HASH)
        .topic1(B256::from(U256::from(batch_number)));

    let logs = l1_provider.get_logs(&filter).await?;
    let log = logs
        .first()
        .ok_or_else(|| anyhow::anyhow!("No BlockCommit event found for batch {batch_number}"))?;

    // BlockCommit(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment)
    // topic[0] = event signature
    // topic[1] = batchNumber
    // topic[2] = batchHash (state commitment)
    let batch_hash = log
        .topics()
        .get(2)
        .ok_or_else(|| anyhow::anyhow!("BlockCommit event missing batchHash topic"))?;

    Ok(*batch_hash)
}
