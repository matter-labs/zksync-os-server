//! Consensus-related functionality.

#![allow(clippy::redundant_locals)]
#![allow(clippy::needless_pass_by_ref_mut)]

use futures::channel::mpsc;
use zksync_concurrency::ctx;


pub use config::*;
pub use storage::rocksdb::ConsensusStorage;
pub use types::ReducedBlockCommand;

mod config;
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
