use crate::model::BlockCommand;
use crate::CHAIN_ID;
use ruint::aliases::U256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zk_os_forward_system::run::BatchContext;

/// Module responsible for providing BatchContext instances on demand.
pub struct BlockContextProvider;

impl BlockContextProvider {
    // TODO: wait for a tx in mempool first ?
    pub async fn get_produce_command(
        &self,
        block_number: u64,
        block_time: Duration,
        block_size_limit: usize,
    ) -> BlockCommand {
        let gas_limit = 100_000_000;
        let timestamp = (millis_since_epoch() / 1000) as u64;
        let context = BatchContext {
            eip1559_basefee: U256::from(1000),
            native_price: U256::from(1),
            gas_per_pubdata: Default::default(),
            block_number,
            timestamp,
            chain_id: CHAIN_ID,
            gas_limit,
            coinbase: Default::default(),
            block_hashes: Default::default(),
        };

        BlockCommand::Produce(context, (block_time, block_size_limit))
    }
}

pub fn millis_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_millis()
}
