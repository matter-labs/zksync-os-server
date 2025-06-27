use anyhow::Context as _;
use futures::channel::mpsc;
use zksync_concurrency::{ctx, error::Wrap as _, scope};
use zksync_consensus_engine::EngineManager;
use zksync_consensus_executor::{self as executor};

use crate::consensus::{
    config,
    config::{ConsensusConfig, ConsensusSecrets},
    storage::{ConsensusStorage, Store},
    types::ReducedBlockCommand,
};

/// Task running a consensus validator for the main node.
/// Main node is currently the only leader of the consensus - i.e. it proposes all the blocks.
pub async fn run_main_node(
    ctx: &ctx::Ctx,
    cfg: ConsensusConfig,
    secrets: ConsensusSecrets,
    storage: ConsensusStorage,
    block_command_sender: mpsc::UnboundedSender<ReducedBlockCommand>,
) -> anyhow::Result<()> {
    let res: ctx::Result<()> = scope::run!(&ctx, |ctx, s| async {
        if let Some(spec) = &cfg.genesis_spec {
            let spec = config::GenesisSpec::parse(spec).context("GenesisSpec::parse()")?;

            storage
                .adjust_global_config(&spec)
                .context("adjust_global_config()")?;
        }

        // Initialize global config.
        let global_config = storage
            .global_config()
            .context("global_config()")?
            .context("global_config() disappeared")?;

        let (store, runner) = Store::new(storage, block_command_sender)
            .await
            .context("Store::new()")?;
        s.spawn_bg(async { Ok(runner.run(ctx).await.context("Store::runner()")?) });

        let (engine_manager, engine_runner) = EngineManager::new(ctx, Box::new(store.clone()))
            .await
            .wrap("BlockStore::new()")?;
        s.spawn_bg(async { Ok(engine_runner.run(ctx).await.context("BlockStore::run()")?) });

        let executor = executor::Executor {
            config: config::executor(&cfg, &secrets, &global_config, None)?,
            engine_manager,
        };

        tracing::info!("running the main node executor");
        executor.run(ctx).await.context("main node executor")?;
        Ok(())
    })
    .await;

    match res {
        Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
        Err(ctx::Error::Internal(err)) => Err(err),
    }
}
