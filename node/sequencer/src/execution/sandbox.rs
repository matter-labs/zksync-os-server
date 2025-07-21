use ruint::aliases::U256;
use zk_os_forward_system::run::errors::ForwardSubsystemError;
use zk_os_forward_system::run::output::TxResult;
use zk_os_forward_system::run::{simulate_tx, BatchContext};
use zksync_os_state::StateView;
use zksync_os_types::{L2Transaction, ZksyncOsEncode};

pub fn execute(
    tx: L2Transaction,
    mut block_context: BatchContext,
    state_view: StateView,
) -> Result<TxResult, ForwardSubsystemError> {
    // tracing::info!(
    //     "Executing transaction: {:?} in block: {:?}",
    //     tx,
    //     block_context.block_number
    // );
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    simulate_tx(encoded_tx, block_context, state_view.clone(), state_view)
}
