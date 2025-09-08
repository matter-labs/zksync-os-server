mod version;

use zksync_os_interface::common_types::{BlockContext, BlockOutput};
use zksync_os_interface::traits::{
    PreimageSource, ReadStorageTree, RunBlock, TxResultCallback, TxSource,
};

pub use crate::version::ZKsyncOSVersion;

pub fn run_block<T: ReadStorageTree, PS: PreimageSource, TS: TxSource, TR: TxResultCallback>(
    zksync_os_version: ZKsyncOSVersion,
    block_context: BlockContext,
    tree: T,
    preimage_source: PS,
    tx_source: TS,
    tx_result_callback: TR,
) -> Result<BlockOutput, anyhow::Error> {
    match zksync_os_version {
        ZKsyncOSVersion::V0_0_21 => zk_os_forward_system::run::RunBlockForward::run_block(
            block_context,
            tree,
            preimage_source,
            tx_source,
            tx_result_callback,
        )
        .map_err(|err| anyhow::anyhow!(err)),
    }
}
