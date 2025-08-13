use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use std::sync::Arc;
use zksync_os_contract_interface::ZkChain;

pub async fn find_l1_block_by_predicate<Fut: Future<Output = anyhow::Result<bool>>>(
    zk_chain: Arc<ZkChain<DynProvider>>,
    predicate: impl Fn(Arc<ZkChain<DynProvider>>, u64) -> Fut,
) -> anyhow::Result<BlockNumber> {
    let latest = zk_chain.provider().get_block_number().await?;

    let guarded_predicate =
        async |zk: Arc<ZkChain<DynProvider>>, block: u64| -> anyhow::Result<bool> {
            if !zk.code_exists_at_block(block.into()).await? {
                // return early if contract is not deployed yet - otherwise `predicate` might fail
                return Ok(false);
            }
            predicate(zk, block).await
        };

    // Ensure the predicate is true by the upper bound, or bail early.
    if !guarded_predicate(zk_chain.clone(), latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    // Binary search on [0, latest] for the first block where predicate is true.
    let (mut lo, mut hi) = (0u64, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if guarded_predicate(zk_chain.clone(), mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}
