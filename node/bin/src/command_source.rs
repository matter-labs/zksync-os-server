use crate::block_replay_storage::BlockReplayStorage;
use crate::command_source;
use crate::replay_transport::replay_receiver;
use async_trait::async_trait;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_pipeline::Source;
use zksync_os_sequencer::model::blocks::BlockCommand;

/// Parameters for the main node command source
#[derive(Debug)]
pub struct MainNodeCommandSourceParams {
    pub block_replay_storage: BlockReplayStorage,
    pub starting_block: u64,
    pub block_time: Duration,
    pub max_transactions_in_block: usize,
}

/// Parameters for the external node command source
#[derive(Debug)]
pub struct ExternalNodeCommandSourceParams {
    pub starting_block: u64,
    pub replay_download_address: String,
}

#[derive(Debug)]
pub struct MainNodeCommandSource;

#[derive(Debug)]
pub struct ExternalNodeCommandSource;

#[async_trait]
impl Source for MainNodeCommandSource {
    type Output = BlockCommand;
    type Params = MainNodeCommandSourceParams;

    const NAME: &'static str = "command_source";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(params: Self::Params, output: mpsc::Sender<Self::Output>) -> anyhow::Result<()> {
        // TODO: no need for a Stream in `command_source` - just send to channel right away instead
        let mut stream = command_source(
            &params.block_replay_storage,
            params.starting_block,
            params.block_time,
            params.max_transactions_in_block,
        );

        while let Some(command) = stream.next().await {
            tracing::debug!(?command, "Sending block command");
            if output.send(command).await.is_err() {
                tracing::warn!("Command output channel closed, stopping source");
                break;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Source for ExternalNodeCommandSource {
    type Output = BlockCommand;
    type Params = ExternalNodeCommandSourceParams;

    const NAME: &'static str = "external_node_command_source";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(params: Self::Params, output: mpsc::Sender<Self::Output>) -> anyhow::Result<()> {
        // TODO: no need for a Stream in `replay_receiver` - just send to channel right away instead
        let mut stream = replay_receiver(
            params.starting_block,
            params.replay_download_address.clone(),
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to connect to main node to receive blocks: {e}");
            e
        })?;

        while let Some(command) = stream.next().await {
            tracing::debug!(?command, "Received block command from main node");
            if output.send(command).await.is_err() {
                tracing::warn!("Command output channel closed, stopping source");
                break;
            }
        }

        Ok(())
    }
}
