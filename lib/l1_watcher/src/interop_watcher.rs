use alloy::primitives::ruint::FromUintError;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::collections::HashMap;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::InteropRoot;
use zksync_os_provider::NodeProvider;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1WatcherError, StartResolver};
use crate::{EventSink, L1WatcherConfig, ProcessRawEvents};

/// Watches interop root updates emitted by L1's MessageRoot and feeds them into the interop
/// subpool.
///
/// Interop roots are built on L1 (era-contracts `MessageRootBase.addChainBatchRoot`), which emits
/// `NewInteropRoot` from the L1 message-root contract. L1-settled chains import these roots for
/// atomic / proof-based interop. The watcher de-duplicates multiple logs for the same `logId` and
/// inserts the latest `IndexedInteropRoot` into its sink.
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    sink: Box<dyn EventSink<IndexedInteropRoot>>,
}

impl InteropWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        l1_bridgehub: Bridgehub<NodeProvider>,
        sink: impl EventSink<IndexedInteropRoot>,
    ) -> anyhow::Result<StartResolver<u64, Self>> {
        // The L1 MessageRoot lives at a deployed address (unlike the canonical L2 address on a
        // gateway); resolve it from the L1 bridgehub.
        let message_root = l1_bridgehub
            .message_root_address()
            .await
            .context("failed to fetch L1 message_root address")?;
        let provider = l1_bridgehub.provider().clone();

        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            ?message_root,
            "initializing interop roots watcher"
        );

        let resolve_start = move |starting_interop_root_id: u64| async move {
            let start_block =
                find_l1_block_by_interop_root_id(l1_bridgehub, starting_interop_root_id)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to find L1 block for \
                             interop_root_id={starting_interop_root_id}"
                        )
                    })?;
            tracing::info!(start_block, "resolved interop roots watcher start on L1");
            let processor = Self {
                starting_interop_root_id,
                sink: Box::new(sink),
            };
            Ok((start_block, processor))
        };

        Ok(StartResolver::new(
            config,
            provider,
            message_root.into(),
            None,
            resolve_start,
        ))
    }
}

#[async_trait::async_trait]
impl ProcessRawEvents for InteropWatcher {
    fn name(&self) -> &'static str {
        "interop_root"
    }

    fn event_signatures(&self) -> Topic {
        NewInteropRoot::SIGNATURE_HASH.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        // we want to accept only the latest event for each log id
        let mut indexes = HashMap::new();

        for log in logs {
            let event = match NewInteropRoot::decode_log(&log.inner) {
                Ok(event) => event.data,
                Err(err) => {
                    tracing::error!(?log, error = ?err, "failed to decode interop root log");
                    continue;
                }
            };
            indexes.insert(event.logId, log);
        }

        indexes.into_values().collect()
    }

    async fn process_raw_event(
        &mut self,
        _provider: &NodeProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let event = NewInteropRoot::decode_log(&log.inner)?.data;

        let log_id: u64 = event
            .logId
            .try_into()
            .map_err(|e: FromUintError<u64>| L1WatcherError::Other(e.into()))?;

        if log_id < self.starting_interop_root_id {
            tracing::debug!(
                log_id,
                starting_interop_root_id = self.starting_interop_root_id,
                "skipping interop root event before starting id",
            );
            return Ok(());
        }
        let interop_root = InteropRoot {
            chainId: event.chainId,
            blockOrBatchNumber: event.blockNumber,
            timestamp: event.timestamp,
            sides: event.sides.clone(),
        };

        self.sink
            .push(IndexedInteropRoot {
                log_id,
                root: interop_root,
            })
            .await;
        Ok(())
    }
}
