//! L1 `MessageRoot` event ingestion for interop-root system transactions.
//!
//! Each `NewInteropRoot` carries a shared root that chains must import before they can verify
//! cross-chain proofs against it. This watcher resumes near the persisted interop cursor, drops
//! roots that were already imported, and forwards new roots to the mempool sink.

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

/// Decodes confirmed `NewInteropRoot` logs for the shared [`L1Watcher`](crate::L1Watcher).
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    sink: Box<dyn EventSink<IndexedInteropRoot>>,
}

impl InteropWatcher {
    /// Creates a resolver so startup can derive the scan block from the replayed interop cursor.
    ///
    /// The resolver resumes at the block containing the cursor, rather than the following block,
    /// so that later roots emitted in the same block are not skipped.
    pub async fn create_watcher(
        config: L1WatcherConfig,
        l1_bridgehub: Bridgehub<NodeProvider>,
        sink: impl EventSink<IndexedInteropRoot>,
    ) -> anyhow::Result<StartResolver<u64, Self>> {
        let provider = l1_bridgehub.provider().clone();
        let message_root = l1_bridgehub
            .message_root_address()
            .await
            .context("failed to fetch L1 message_root address for interop watcher")?;

        let resolve_start = move |starting_interop_root_id: u64| async move {
            let start_block =
                find_l1_block_by_interop_root_id(l1_bridgehub.clone(), starting_interop_root_id)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to find L1 block for interop_root_id={starting_interop_root_id}"
                        )
                    })?;
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
        // A polling range may contain repeated updates for one log id. Only its latest root should
        // reach the subpool.
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

        // Because startup rescans the block containing the cursor, only that first scanned L1 block
        // can contain roots that were already imported.
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
