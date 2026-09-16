//! L1 `MessageRoot` event ingestion for interop-root system transactions.
//!
//! Each `NewInteropRoot` carries a shared root that chains must import before they can verify
//! cross-chain proofs against it. This watcher resumes near the persisted interop cursor, drops
//! roots that were already imported, and forwards new roots to the mempool sink.

use crate::metrics::METRICS;
use alloy::primitives::ruint::FromUintError;
use alloy::primitives::{ChainId, U256};
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::collections::{HashMap, HashSet};
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::InteropRoot;
use zksync_os_provider::NodeProvider;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1WatcherError, StartResolver};
use crate::{EventSink, L1WatcherConfig, ProcessRawEvents};

/// Which chains' interop roots a node imports out of the shared `MessageRoot`.
///
/// Every imported root becomes an `ImportInteropRoots` system transaction, and those seal a batch,
/// so a chain that receives no interop traffic still proves a batch for every root every other
/// chain publishes. Narrowing the sources is what keeps that cost proportional to the interop a
/// chain actually expects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InteropRootSources {
    /// Import every root published on L1.
    #[default]
    All,
    /// Import roots published by these chains only.
    Only(HashSet<ChainId>),
    /// Import nothing. The watcher is not started at all, so no L1 scanning happens either.
    None,
}

impl InteropRootSources {
    fn accepts(&self, chain_id: U256) -> bool {
        match self {
            Self::All => true,
            Self::Only(chains) => chain_id
                .try_into()
                .is_ok_and(|chain_id: ChainId| chains.contains(&chain_id)),
            Self::None => false,
        }
    }
}

/// Decodes confirmed `NewInteropRoot` logs for the shared [`L1Watcher`](crate::L1Watcher).
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    sources: InteropRootSources,
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
        sources: InteropRootSources,
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
                sources,
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

        // A root this chain does not import is dropped here rather than in the mempool, so it never
        // reaches a block and never seals a batch. The interop cursor therefore does not advance
        // past it: re-widening the sources later replays everything skipped in between.
        if !self.sources.accepts(event.chainId) {
            tracing::debug!(
                log_id,
                chain_id = %event.chainId,
                "skipping interop root from a chain this node does not import from",
            );
            METRICS.interop_roots_skipped.inc();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_filter_by_chain_id() {
        assert!(InteropRootSources::All.accepts(U256::from(271)));
        assert!(!InteropRootSources::None.accepts(U256::from(271)));

        let only = InteropRootSources::Only(HashSet::from([271]));
        assert!(only.accepts(U256::from(271)));
        assert!(!only.accepts(U256::from(300)));
        // A chain id that does not fit a `ChainId` cannot be listed, so it is never imported.
        assert!(!only.accepts(U256::MAX));
    }
}
