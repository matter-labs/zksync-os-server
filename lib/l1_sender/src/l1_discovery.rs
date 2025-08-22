use crate::config::BatchDaInputMode;
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::DynProvider;
use zksync_os_contract_interface::{Bridgehub, PubdataPricingMode};

#[derive(Debug, Clone)]
pub struct L1State {
    pub bridgehub: Address,
    pub diamond_proxy: Address,
    pub validator_timelock: Address,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
    pub da_input_mode: BatchDaInputMode,
}

pub async fn get_l1_state(
    provider: &DynProvider,
    bridgehub_address: Address,
    chain_id: u64,
) -> anyhow::Result<L1State> {
    let bridgehub = Bridgehub::new(bridgehub_address, provider, chain_id);
    let all_chain_ids = bridgehub.get_all_zk_chain_chain_ids().await?;
    anyhow::ensure!(
        all_chain_ids.contains(&U256::from(chain_id)),
        "chain ID {} is not registered on L1",
        chain_id
    );
    let zk_chain = bridgehub.zk_chain().await?;
    let diamond_proxy = *zk_chain.address();
    let validator_timelock_address = bridgehub.validator_timelock_address().await?;

    let last_committed_batch = zk_chain
        .get_total_batches_committed(BlockId::latest())
        .await?;

    let last_proved_batch = zk_chain
        .get_total_batches_proved()
        .await?
        .saturating_to::<u64>();

    let last_executed_batch = zk_chain
        .get_total_batches_executed(BlockId::latest())
        .await?;

    let pubdata_pricing_mode = zk_chain.get_pubdata_pricing_mode().await?;
    let da_input_mode = match pubdata_pricing_mode {
        PubdataPricingMode::Rollup => BatchDaInputMode::Rollup,
        PubdataPricingMode::Validium => BatchDaInputMode::Validium,
        v => panic!("Unexpected pubdata pricing mode: {}", v as u8),
    };

    Ok(L1State {
        bridgehub: bridgehub_address,
        diamond_proxy,
        validator_timelock: validator_timelock_address,
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
        da_input_mode,
    })
}
