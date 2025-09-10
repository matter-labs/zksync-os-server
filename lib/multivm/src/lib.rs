//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new protocol version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_PROTOCOL_VERSION` constant accordingly.

use zk_os_forward_system::run::RunBlockForward;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

pub fn run_block<S: ReadStorage, PS: PreimageSource, TS: TxSource, TR: TxResultCallback>(
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
    tx_source: TS,
    tx_result_callback: TR,
) -> Result<BlockOutput, anyhow::Error> {
    match block_context.protocol_version {
        1 => {
            let object = RunBlockForward {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        v => panic!("Unsupported protocol version: {v}"),
    }
}

pub fn simulate_tx<S: ReadStorage, PS: PreimageSource>(
    transaction: Vec<u8>,
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    match block_context.protocol_version {
        1 => {
            let object = RunBlockForward {};
            object
                .simulate_tx((), transaction, block_context, storage, preimage_source)
                .map_err(|err| anyhow::anyhow!(err))
        }
        v => panic!("Unsupported protocol version: {v}"),
    }
}

pub const LATEST_PROTOCOL_VERSION: u32 = 1;
