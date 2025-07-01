use crate::conversions::tx_abi_encode;
use ruint::aliases::U256;
use zk_os_forward_system::run::{simulate_tx, BatchContext, TxOutput};
use zksync_os_state::StateView;
use zksync_types::l2::L2Tx;

pub fn execute(
    tx: L2Tx,
    mut block_context: BatchContext,
    state_view: StateView,
) -> anyhow::Result<TxOutput> {
    // tracing::info!(
    //     "Executing transaction: {:?} in block: {:?}",
    //     tx,
    //     block_context.block_number
    // );
    let encoded_tx = tx_abi_encode(tx.into());

    block_context.eip1559_basefee = U256::from(0);

    simulate_tx(encoded_tx, block_context, state_view.clone(), state_view)
        .map_err(|e| anyhow::anyhow!("{e:?}"))? // outer error
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}
