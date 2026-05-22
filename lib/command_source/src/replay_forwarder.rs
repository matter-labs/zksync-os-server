use tokio::sync::mpsc;
use zksync_os_backpressure::PipelineAdmissionReceiver;
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::ReplayRecord;

/// Shared forwarding path for replay commands from local WAL, consensus, and EN sync.
#[derive(Debug)]
pub struct ReplayCommandForwarder {
    pipeline_gate: PipelineAdmissionReceiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForwardedReplay {
    pub block_number: u64,
    pub timestamp: u64,
}

impl ReplayCommandForwarder {
    pub fn new(pipeline_gate: PipelineAdmissionReceiver) -> Self {
        Self { pipeline_gate }
    }

    pub fn is_open(&self) -> bool {
        self.pipeline_gate.is_open()
    }

    pub async fn wait_until_open(&mut self) -> anyhow::Result<()> {
        self.pipeline_gate.wait_until_open().await
    }

    pub(crate) async fn forward(
        &self,
        record: ReplayRecord,
        output: &mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<ForwardedReplay> {
        let block_number = record.block_context.block_number;
        let timestamp = record.block_context.timestamp;
        output
            .send(BlockCommand::Replay(Box::new(record)))
            .await
            .map_err(|_| anyhow::anyhow!("command output channel closed"))?;

        Ok(ForwardedReplay {
            block_number,
            timestamp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, BlockNumber};
    use zksync_os_backpressure::PipelineAdmissionGate;
    use zksync_os_interface::types::BlockHashes;
    use zksync_os_types::{BlockStartCursors, ProtocolSemanticVersion};

    fn replay_record(block_number: BlockNumber) -> ReplayRecord {
        ReplayRecord::new(
            zksync_os_storage_api::BlockContext {
                chain_id: 270,
                block_number,
                block_hashes: BlockHashes::default(),
                timestamp: block_number + 1,
                eip1559_basefee: Default::default(),
                pubdata_price: Default::default(),
                native_price: Default::default(),
                coinbase: Default::default(),
                gas_limit: 1_000_000,
                pubdata_limit: 1_000_000,
                mix_hash: Default::default(),
                execution_version: 1,
                blob_fee: Default::default(),
            },
            vec![],
            block_number,
            semver::Version::new(0, 0, 0),
            ProtocolSemanticVersion::new(0, 31, 0),
            B256::with_last_byte(block_number as u8),
            vec![],
            BlockStartCursors::default(),
        )
    }

    #[tokio::test]
    async fn replay_command_forwarder_sends_replay_command() {
        let (_gate, pipeline_gate) = PipelineAdmissionGate::open();
        let forwarder = ReplayCommandForwarder::new(pipeline_gate);
        let (output_tx, mut output_rx) = mpsc::channel(1);

        let forwarded = forwarder
            .forward(replay_record(7), &output_tx)
            .await
            .expect("replay should be forwarded");

        assert_eq!(forwarded.block_number, 7);
        assert_eq!(forwarded.timestamp, 8);

        let command = output_rx
            .recv()
            .await
            .expect("forwarded command should be available");
        let BlockCommand::Replay(record) = command else {
            panic!("expected replay command");
        };
        assert_eq!(record.block_context.block_number, 7);
    }
}
