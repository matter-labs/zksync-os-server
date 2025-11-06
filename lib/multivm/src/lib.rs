//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_EXECUTION_VERSION` constant accordingly.

use num_enum::TryFromPrimitive;
use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV3;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::AnyTracer;
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

pub mod apps;

#[derive(Debug, Clone, Copy, TryFromPrimitive, PartialEq)]
#[repr(u32)]
pub enum ExecutionVersion {
    V1 = 1,
    V2 = 2,
    V3 = 3,
}

impl ExecutionVersion {
    // NOTE: V1 and V2 have a slight chance of being off as they've been backfilled.
    // If you find a divergence in what you expect and the actual value, most likely a bug.

    /// verification key hash generated from zksync-os v0.0.21, zksync-airbender v0.4.4 and zkos-wrapper v0.4.3
    const V1_VK_HASH: &'static str =
        "0x80a72fbdf9d6ab299fb5dfc2bcc807cfc7be38c9cfb0bc9b1ce6f9510fb110ea";
    /// verification key hash generated from zksync-os v0.0.25, zksync-airbender v0.4.5 and zkos-wrapper v0.4.6
    const V2_VK_HASH: &'static str =
        "0x83d49897775e6c1f1d7247ec228e18158e8e3accda545c604de4c44eee1a9845";
    /// verification key hash generated from zksync-os v0.0.26, zksync-airbender v0.5.0 and zkos-wrapper v0.5.0
    const V3_VK_HASH: &'static str =
        "0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3";

    /// Get the verification key hash associated with this execution version.
    pub fn vk_hash(&self) -> &'static str {
        match self {
            ExecutionVersion::V1 => Self::V1_VK_HASH,
            ExecutionVersion::V2 => Self::V2_VK_HASH,
            ExecutionVersion::V3 => Self::V3_VK_HASH,
        }
    }

    /// Try to get ExecutionVersion from verification key hash.
    pub fn try_from_vk_hash(vk_hash: &str) -> anyhow::Result<Self> {
        match vk_hash {
            Self::V1_VK_HASH => Ok(ExecutionVersion::V1),
            Self::V2_VK_HASH => Ok(ExecutionVersion::V2),
            Self::V3_VK_HASH => Ok(ExecutionVersion::V3),
            val => Err(anyhow::anyhow!("unknown verification key hash: {val}")),
        }
    }
}

pub const LATEST_EXECUTION_VERSION: ExecutionVersion = ExecutionVersion::V3;

pub fn run_block<
    Storage: ReadStorage,
    PreimgSrc: PreimageSource,
    TrSrc: TxSource,
    TrCallback: TxResultCallback,
    Tracer: AnyTracer,
>(
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tx_source: TrSrc,
    tx_result_callback: TrCallback,
    tracer: &mut Tracer,
) -> Result<BlockOutput, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                    tracer,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: AnyTracer>(
    transaction: Vec<u8>,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .simulate_tx(
                    (),
                    transaction,
                    block_context,
                    storage,
                    preimage_source,
                    tracer,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
    }
}

/// Method to decide what execution version/VK should the prover use.
///
/// Generally speaking, we could have a single execution version, the one used by the server.
/// There's an edge case where we have a circuit bug and it would require us to prove a batch
/// with a different execution version than the one it was generated/sealed.
/// For such cases, we have this mapping defined here, that will allow us to release patch versions that will say:
/// "oh, you executed with v3? np, prove with v4 as v3 proving is bugged".
pub fn proving_run_execution_version(forward_run_execution_version: u32) -> ExecutionVersion {
    let forward_run_execution_version: ExecutionVersion = forward_run_execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match forward_run_execution_version {
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => ExecutionVersion::V3,
    }
}
