use crate::commands::{L1SenderCommand, commit::CommitCommand};
use alloy::{eips::BlockId, providers::DynProvider};
use anyhow::Context as _;
use async_trait::async_trait;
use std::cmp::Ordering;
use zksync_os_contract_interface::ZkChain;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PipelineComponent, TrackedUnboundedReceiver, TrackedUnboundedSender};
use zksync_os_types::ProtocolSemanticVersion;

/// Receives Batches with proofs - potentially with incompatible protocol version.
/// Makes sure that batches are only passed to L1 if batch version matches the current protocol version.
#[derive(Debug)]
pub struct UpgradeGatekeeper {
    zk_chain_sl: ZkChain<DynProvider>,
    pub health_reporter: ComponentHealthReporter,
}

impl UpgradeGatekeeper {
    pub fn new(
        zk_chain_sl: ZkChain<DynProvider>,
        health_reporter: ComponentHealthReporter,
    ) -> Self {
        Self {
            zk_chain_sl,
            health_reporter,
        }
    }
}

async fn current_protocol_version(
    zk_chain_sl: &ZkChain<DynProvider>,
) -> anyhow::Result<ProtocolSemanticVersion> {
    let current_protocol_version = zk_chain_sl
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
    zk_chain_sl: &ZkChain<DynProvider>,
    target_protocol_version: &ProtocolSemanticVersion,
) -> anyhow::Result<()> {
    let mut current_pv = current_protocol_version(zk_chain_sl).await?;
    tracing::info!(
        %current_pv,
        %target_protocol_version,
        "Waiting for L1 protocol version {current_pv} to reach target version {target_protocol_version}",
    );
    loop {
        match current_pv.cmp(target_protocol_version) {
            Ordering::Greater => {
                // We don't expect protocol version on L1 to be greater than the version of non-committed
                // batch, it's an unexpected hard error.
                anyhow::bail!(
                    "Protocol version on the contract {current_pv} is greater than protocol version for the next uncommitted batch: {target_protocol_version}"
                );
            }
            Ordering::Equal => {
                tracing::info!(
                    %current_pv,
                    "Protocol version on the contract matches batch protocol version"
                );
                return Ok(());
            }
            Ordering::Less => {
                tracing::debug!(
                    %current_pv,
                    %target_protocol_version,
                    "Protocol version on L1 is still less than target version, waiting"
                );
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
        current_pv = current_protocol_version(zk_chain_sl).await?;
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
        mut input: TrackedUnboundedReceiver<Self::Input>,
        output: TrackedUnboundedSender<Self::Output>,
    ) -> anyhow::Result<()> {
        let UpgradeGatekeeper {
            zk_chain_sl,
            health_reporter,
        } = self;

        loop {
            health_reporter.enter_state(GenericComponentState::Idle);
            let Some(command) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            if let L1SenderCommand::SendToL1(command) = &command {
                health_reporter.enter_state(GenericComponentState::Active);

                let batch_protocol_version = command.input().batch.protocol_version.clone();

                // `self` was destructured above, so `zk_chain_sl` is a plain local variable here.
                // The free function `wait_until_protocol_version` takes `&ZkChain<DynProvider>`
                // directly, which matches that local — no `self` receiver is needed or available.
                wait_until_protocol_version(&zk_chain_sl, &batch_protocol_version).await?;
            }

            let last_block = command.last_block_number();
            let last_block_timestamp = match &command {
                L1SenderCommand::SendToL1(cmd) => cmd
                    .as_ref()
                    .last()
                    .map(|e| e.batch.batch_info.last_block_timestamp),
                L1SenderCommand::Passthrough(envelope) => {
                    Some(envelope.batch.batch_info.last_block_timestamp)
                }
            };
            if output.send(command).is_err() {
                anyhow::bail!("Outbound channel closed");
            }
            health_reporter.record_processed(last_block, last_block_timestamp);
        }
    }
}
