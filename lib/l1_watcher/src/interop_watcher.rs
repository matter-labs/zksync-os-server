use alloy::rpc::types::Log;
use alloy::{primitives::Address, providers::DynProvider};
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMessageRoot::AppendedChainRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot};
use zksync_os_types::{IndexedInteropRootsEnvelope, InteropRootsEnvelope, InteropRootsLogIndex};

use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};

pub struct InteropWatcher {
    contract_address: Address,
    output: mpsc::Sender<IndexedInteropRootsEnvelope>,
    starting_interop_event_index: InteropRootsLogIndex,
}

impl InteropWatcher {
    pub async fn create_watcher(
        bridgehub: Bridgehub<DynProvider>,
        config: L1WatcherConfig,
        output: mpsc::Sender<IndexedInteropRootsEnvelope>,
        starting_interop_event_index: InteropRootsLogIndex,
    ) -> anyhow::Result<L1Watcher> {
        let contract_address = bridgehub.message_root_address().await?;

        tracing::info!(
            contract_address = ?contract_address,
            starting_interop_event_index = ?starting_interop_event_index,
            "initializing interop watcher"
        );

        let this = Self {
            contract_address,
            output,
            starting_interop_event_index,
        };

        let l1_watcher = L1Watcher::new(
            bridgehub.provider().clone(),
            this.starting_interop_event_index.block_number,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );

        Ok(l1_watcher)
    }
}

#[async_trait::async_trait]
impl ProcessL1Event for InteropWatcher {
    const NAME: &'static str = "interop_root";

    type SolEvent = AppendedChainRoot;
    type WatchedEvent = AppendedChainRoot;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(
        &mut self,
        tx: AppendedChainRoot,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let current_log_index = InteropRootsLogIndex {
            block_number: log.block_number.expect("Block number is required"),
            index_in_block: log.log_index.expect("Log index is required"),
        };

        if current_log_index < self.starting_interop_event_index {
            tracing::debug!(
                current_log_index = ?current_log_index,
                starting_interop_event_index = ?self.starting_interop_event_index,
                "skipping interop root event before starting index",
            );
            return Ok(());
        }

        let interop_root = InteropRoot {
            chainId: tx.chainId,
            blockOrBatchNumber: tx.batchNumber,
            sides: vec![tx.chainRoot],
        };

        let indexed_envelope = IndexedInteropRootsEnvelope {
            log_index: current_log_index,
            envelope: InteropRootsEnvelope::from_interop_roots(vec![interop_root]),
        };

        self.output
            .send(indexed_envelope)
            .await
            .map_err(|_| L1WatcherError::OutputClosed)?;

        Ok(())
    }
}
