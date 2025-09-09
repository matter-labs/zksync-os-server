mod version;

use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::{BlockContext, BlockOutput, TxOutput};

pub use crate::version::ZKsyncOSVersion;

pub fn run_block<S: ReadStorage, PS: PreimageSource, TS: TxSource, TR: TxResultCallback>(
    zksync_os_version: ZKsyncOSVersion,
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
    tx_source: TS,
    tx_result_callback: TR,
) -> Result<BlockOutput, anyhow::Error> {
    match zksync_os_version {
        ZKsyncOSVersion::V0_0_21 => zk_os_forward_system::run::RunBlockForward::run_block(
            block_context,
            storage,
            preimage_source,
            tx_source,
            tx_result_callback,
        )
        .map_err(|err| anyhow::anyhow!(err)),
    }
}

pub fn simulate_tx<S: ReadStorage, PS: PreimageSource>(
    zksync_os_version: ZKsyncOSVersion,
    transaction: Vec<u8>,
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    match zksync_os_version {
        ZKsyncOSVersion::V0_0_21 => zk_os_forward_system::run::RunBlockForward::simulate_tx(
            transaction,
            block_context,
            storage,
            preimage_source,
        )
        .map_err(|err| anyhow::anyhow!(err)),
    }
}
