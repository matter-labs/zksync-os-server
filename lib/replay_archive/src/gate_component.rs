use crate::ReplayArchiver;
use crate::metrics::REPLAY_ARCHIVE_METRICS;
use alloy::primitives::B256;
use anyhow::Context;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::ReadReplay;

pub struct ReplayArchiveGateComponent<Archive, ReplayStorage> {
    archive: Archive,
    replay_storage: ReplayStorage,
}

impl<Archive, ReplayStorage> ReplayArchiveGateComponent<Archive, ReplayStorage>
where
    Archive: ReplayArchiver,
    ReplayStorage: ReadReplay,
{
    pub fn new(archive: Archive, replay_storage: ReplayStorage) -> Self {
        Self {
            archive,
            replay_storage,
        }
    }

    async fn wait_for_archive_to_contain_block(
        &self,
        block_number: u64,
        block_hash: B256,
    ) -> anyhow::Result<()> {
        const POLL_INTERVAL: Duration = Duration::from_secs(1);

        let started_at = Instant::now();
        let mut timer = tokio::time::interval(POLL_INTERVAL);
        loop {
            timer.tick().await;

            if self
                .archive
                .contains_replay_record(block_number, block_hash)
                .await?
            {
                REPLAY_ARCHIVE_METRICS
                    .gate_wait
                    .observe(started_at.elapsed());
                return Ok(());
            }
        }
    }
}

#[async_trait::async_trait]
impl<Archive, ReplayStorage> PipelineComponent
    for ReplayArchiveGateComponent<Archive, ReplayStorage>
where
    Archive: ReplayArchiver,
    ReplayStorage: ReadReplay,
{
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::ReplayArchiveGate;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);

            let Some(item) = input.recv_and_record_picked(&state_reporter).await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            state_reporter.enter_state(GenericComponentState::Active);

            let envelope = match &item {
                L1SenderCommand::SendToL1(command) => &command.input,
                L1SenderCommand::Passthrough(envelope) => envelope,
            };
            let block_range = envelope.batch.first_block_number..=envelope.batch.last_block_number;
            tracing::info!(
                "Entered {} for batch #{}, block range {block_range:?}",
                Self::COMPONENT_ID.as_str(),
                envelope.batch.batch_info.batch_number
            );

            // Iterates in reverse order so that block hash can be tracked easily.
            let mut block_hash = envelope
                .batch
                .last_block_hash
                .expect("Missing last_block_hash");
            for block_number in block_range.rev() {
                let replay_record = self
                    .replay_storage
                    .get_replay_record(block_number)
                    .with_context(|| format!("replay record {block_number}"))?;
                self.wait_for_archive_to_contain_block(block_number, block_hash)
                    .await?;

                // assign prev block hash.
                block_hash = replay_record.block_context.block_hashes.0[255]
                    .to_be_bytes()
                    .into();
            }

            output.send_and_record(item, &state_reporter)?;
        }
    }
}
