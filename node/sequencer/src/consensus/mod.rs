//! Consensus-related functionality.

#![allow(clippy::redundant_locals)]
#![allow(clippy::needless_pass_by_ref_mut)]

use futures::channel::mpsc;
use zksync_concurrency::ctx;


pub use config::*;
pub use storage::rocksdb::ConsensusStorage;
pub use types::ReducedBlockCommand;

mod config;
// mod en;
mod mn;
mod conv;
mod proto;
mod storage;
mod types;

/// Runs the consensus task in the main node mode.
pub async fn run_main_node(
    ctx: &ctx::Ctx,
    cfg: ConsensusConfig,
    secrets: ConsensusSecrets,
    storage: ConsensusStorage,
    block_command_sender: mpsc::UnboundedSender<ReducedBlockCommand>,
) -> anyhow::Result<()> {
    tracing::info!(
        is_validator = secrets.validator_key.is_some(),
        "running main node"
    );

    // For now in case of error we just log it and allow the server
    // to continue running.
    if let Err(err) = mn::run_main_node(ctx, cfg, secrets, storage, block_command_sender).await {
        tracing::error!("Consensus actor failed: {err:#}");
    } else {
        tracing::info!("Consensus actor stopped");
    }
    Ok(())
}

// /// Runs the consensus node for the external node.
// /// If `cfg` is `None`, it will just fetch blocks from the main node
// /// using JSON RPC, without starting the consensus node.
// pub async fn run_external_node(
//     ctx: &ctx::Ctx,
//     cfg: Option<(ConsensusConfig, ConsensusSecrets)>,
//     pool: zksync_dal::ConnectionPool<Core>,
//     sync_state: SyncState,
//     main_node_client: Box<DynClient<L2>>,
//     actions: ActionQueueSender,
//     build_version: semver::Version,
// ) -> anyhow::Result<()> {
//     let en = en::EN {
//         pool: storage::ConnectionPool(pool),
//         sync_state: sync_state.clone(),
//         client: main_node_client.for_component("block_fetcher"),
//     };
//     let res = match cfg {
//         Some((cfg, secrets)) => {
//             tracing::info!(
//                 is_validator = secrets.validator_key.is_some(),
//                 "running external node"
//             );
//             en.run(ctx, actions, cfg, secrets, Some(build_version))
//                 .await
//         }
//         None => {
//             tracing::info!("running fetcher");
//             en.run_fetcher(ctx, actions).await
//         }
//     };
//     tracing::info!("Consensus actor stopped");
//     res
// }
