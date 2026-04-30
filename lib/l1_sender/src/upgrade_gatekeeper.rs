use crate::commands::{L1SenderCommand, commit::CommitCommand};
use alloy::{eips::BlockId, providers::DynProvider};
use anyhow::Context as _;
use async_trait::async_trait;
use std::cmp::Ordering;
use tokio::sync::mpsc;
use zksync_os_contract_interface::ZkChain;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_types::ProtocolSemanticVersion;

/// Receives Batches with proofs - potentially with incompatible protocol version.
/// Makes sure that batches are only passed to L1 if batch version matches the current protocol version.
#[derive(Debug)]
pub struct UpgradeGatekeeper {
    zk_chain_sl: ZkChain<DynProvider>,
}

impl UpgradeGatekeeper {
    pub fn new(zk_chain_sl: ZkChain<DynProvider>) -> Self {
        Self { zk_chain_sl }
    }

    async fn current_protocol_version(&self) -> anyhow::Result<ProtocolSemanticVersion> {
        let current_protocol_version = self
            .zk_chain_sl
            .get_raw_protocol_version(BlockId::latest())
            .await
            .context("Failed to fetch current protocol version from L1")?;
        let current_protocol_version =
            ProtocolSemanticVersion::try_from(current_protocol_version).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid protocol version fetched from L1: {e}; protocol_version: {current_protocol_version}"
                )
            })?;
        Ok(current_protocol_version)
    }

    async fn wait_until_protocol_version(
        &self,
        target_protocol_version: &ProtocolSemanticVersion,
    ) -> anyhow::Result<()> {
        let mut current_protocol_version = self.current_protocol_version().await?;
        tracing::info!(
            %current_protocol_version,
            %target_protocol_version,
            "Waiting for L1 protocol version {current_protocol_version} to reach target version {target_protocol_version}",
        );
        loop {
            match current_protocol_version.cmp(target_protocol_version) {
                Ordering::Greater => {
                    // We don't expect protocol version on L1 to be greater than the version of non-committed
                    // batch, it's an unexpected hard error.
                    anyhow::bail!(
                        "Protocol version on the contract {current_protocol_version} is greater than protocol version for the next uncommitted batch: {target_protocol_version}"
                    );
                }
                Ordering::Equal => {
                    tracing::info!(
                        %current_protocol_version,
                        "Protocol version on the contract matches batch protocol version"
                    );
                    return Ok(());
                }
                Ordering::Less => {
                    tracing::debug!(
                        %current_protocol_version,
                        %target_protocol_version,
                        "Protocol version on L1 is still less than target version, waiting"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
            current_protocol_version = self.current_protocol_version().await?;
        }
    }
}

#[async_trait]
impl PipelineComponent for UpgradeGatekeeper {
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::UpgradeGatekeeper;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            let Some(command) = input.recv_and_record_picked(&state_reporter).await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            if let L1SenderCommand::SendToL1(command) = &command {
                state_reporter.enter_state(GenericComponentState::Active);

                let batch_protocol_version =
                    command.input().batch.batch_info.protocol_version.clone();
                self.wait_until_protocol_version(&batch_protocol_version)
                    .await?;
            }

            output.send_and_record(command, &state_reporter)?;
        }
    }
}
