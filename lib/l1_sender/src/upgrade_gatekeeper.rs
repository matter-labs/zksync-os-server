use crate::commands::{L1SenderCommand, commit::CommitCommand};
use alloy::providers::DynProvider;
use anyhow::Context as _;
use async_trait::async_trait;
use std::cmp::Ordering;
use tokio::sync::mpsc;
use zksync_os_contract_interface::ZkChain;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_types::ProtocolSemanticVersion;

/// Receives Batches with proofs - potentially with incompatible protocol version.
/// Makes sure that batches are only passed to L1 if batch version matches the current protocol version.
#[derive(Debug)]
pub struct UpgradeGatekeeper {
    zk_chain: ZkChain<DynProvider>,
}

impl UpgradeGatekeeper {
    pub fn new(zk_chain: ZkChain<DynProvider>) -> Self {
        Self { zk_chain }
    }

    async fn current_protocol_version(&self) -> anyhow::Result<ProtocolSemanticVersion> {
        let current_protocol_version = self
            .zk_chain
            .get_raw_protocol_version()
            .await
            .context("Failed to fetch current protocol version from L1")?; // TODO: handle transient errors
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
        loop {
            let current_protocol_version = self.current_protocol_version().await?;
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
                        "Protocol version on the contract {current_protocol_version} matches batch protocol version"
                    );
                    return Ok(());
                }
                Ordering::Less => {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await; // TODO: do we want to make it configurable?
                }
            }
        }
    }
}

#[async_trait]
impl PipelineComponent for UpgradeGatekeeper {
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const NAME: &'static str = "upgrade_gatekeeper";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("upgrade_gatekeeper", GenericComponentState::WaitingRecv);

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let Some(command) = input.recv().await else {
                anyhow::bail!("UpgradeGatekeeper input stream ended unexpectedly");
            };

            if let L1SenderCommand::SendToL1(command) = &command {
                latency_tracker.enter_state(GenericComponentState::Processing);

                let batch_protocol_version = command.input().batch.protocol_version.clone();

                self.wait_until_protocol_version(&batch_protocol_version)
                    .await?;
            }

            output.send(command).await?;
        }
    }
}
