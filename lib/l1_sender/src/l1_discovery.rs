use crate::config::L1SenderConfig;
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::DynProvider;
use zksync_os_contract_interface::Bridgehub;

#[derive(Debug, Clone)]
pub struct L1State {
    pub bridgehub: Address,
    pub diamond_proxy: Address,
    pub validator_timelock: Address,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
}

pub async fn get_l1_state(
    provider: &DynProvider,
    config: L1SenderConfig,
    // todo: consider getting rid of GenesisConfig and putting this inside L1Config
    chain_id: u64,
) -> anyhow::Result<L1State> {
    let bridgehub = Bridgehub::new(config.bridgehub_address.0.into(), provider, chain_id);
    let all_chain_ids = bridgehub.get_all_zk_chain_chain_ids().await?;
    anyhow::ensure!(
        all_chain_ids.contains(&U256::from(chain_id)),
        "chain ID {} is not registered on L1",
        chain_id
    );
    let zk_chain = bridgehub.zk_chain().await?;
    let diamond_proxy = *zk_chain.address();
    let validator_timelock_address = bridgehub.validator_timelock_address().await?;

    let last_committed_batch = bridgehub
        .zk_chain()
        .await?
        .get_total_batches_committed(BlockId::latest())
        .await?;

    let last_proved_batch = bridgehub
        .zk_chain()
        .await?
        .get_total_batches_proved()
        .await?
        .saturating_to::<u64>();

    let last_executed_batch = bridgehub
        .zk_chain()
        .await?
        .get_total_batches_executed()
        .await?
        .saturating_to::<u64>();

    Ok(L1State {
        bridgehub: config.bridgehub_address,
        diamond_proxy,
        validator_timelock: validator_timelock_address,
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
    })
}
