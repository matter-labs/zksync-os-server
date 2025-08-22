use alloy::primitives::U256;
use zk_ee::system::tracer::NopTracer;
use zk_os_forward_system::run::errors::ForwardSubsystemError;
use zk_os_forward_system::run::output::TxResult;
use zk_os_forward_system::run::{BlockContext, simulate_tx};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{L2Transaction, ZksyncOsEncode};

pub fn execute(
    tx: L2Transaction,
    mut block_context: BlockContext,
    state_view: impl ViewState,
) -> Result<TxResult, Box<ForwardSubsystemError>> {
    // tracing::info!(
    //     "Executing transaction: {:?} in block: {:?}",
    //     tx,
    //     block_context.block_number
    // );
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut NopTracer::default(),
    )
    .map_err(Box::new)
}
