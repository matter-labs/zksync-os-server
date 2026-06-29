//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_EXECUTION_VERSION` constant accordingly.

use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV6;
// NOTE (native-transfers bench, TEMPORARY): pre-0.3.0 execution versions (V1–V5) are dropped so the
// whole graph can build against the modified (3-field `EncodedTx`) interface. Only the current V6
// forward system is wired; multivm errors on older versions (the bench chain runs at V6). Restore the
// historical `RunBlockForwardV3/V4/V5*` imports + dispatch arms before merging.
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, AnyTxValidator};
use zksync_os_interface::traits::{
    EncodedTx, NoFriProofSidecar, PreimageSource, ReadStorage, RunBlock, SimulateTx,
    TxResultCallback, TxSource,
};
use zksync_os_interface::types::TxOutput;
use zksync_os_storage_api::BlockContext;

mod adapter;
pub mod apps;

pub use adapter::AbiTxSource;
use zksync_os_types::{BlockOutput, ExecutionVersion};
macro_rules! into_block_output {
    ($o:expr) => {
        BlockOutput {
            header: $o.header,
            tx_results: $o.tx_results,
            storage_writes: $o.storage_writes,
            account_diffs: $o.account_diffs,
            published_preimages: $o.published_preimages,
            pubdata: $o.pubdata,
            computational_native_used: $o.computational_native_used,
        }
    };
}

pub fn run_block<
    Storage: ReadStorage,
    PreimgSrc: PreimageSource,
    TrSrc: TxSource,
    TrCallback: TxResultCallback,
    Tracer: AnyTracer,
    Validator: AnyTxValidator,
>(
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tx_source: TrSrc,
    tx_result_callback: TrCallback,
    tracer: &mut Tracer,
    validator: &mut Validator,
) -> Result<BlockOutput, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        ExecutionVersion::V6 => {
            let object = RunBlockForwardV6 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    NoFriProofSidecar,
                    tx_result_callback,
                    tracer,
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
                .map(|o| into_block_output!(o))
        }
        other => Err(anyhow::anyhow!(
            "pre-0.3.0 execution version {other:?} is not supported in this native-transfers bench build"
        )),
    }
}

pub fn simulate_tx<
    Storage: ReadStorage,
    PreimgSrc: PreimageSource,
    Tracer: AnyTracer,
    Validator: AnyTxValidator,
>(
    transaction: EncodedTx,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
    validator: &mut Validator,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        ExecutionVersion::V6 => {
            let object = RunBlockForwardV6 {};
            object
                .simulate_tx(
                    (),
                    transaction,
                    block_context,
                    storage,
                    preimage_source,
                    tracer,
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        other => Err(anyhow::anyhow!(
            "pre-0.3.0 execution version {other:?} is not supported in this native-transfers bench build"
        )),
    }
}
