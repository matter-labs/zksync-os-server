use alloy::primitives::ruint::FromUintError;
use alloy::primitives::{Address, address};
use alloy::providers::DynProvider;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::collections::HashMap;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::InteropRoot;
use zksync_os_contract_interface::settlement_layer_intervals::{
    IntervalSettlementLayer, SettlementLayerIntervals,
};
use zksync_os_mempool::subpools::interop_roots::InteropRootsSubpool;
use zksync_os_types::IndexedInteropRoot;

use crate::sl_aware_watcher::{SegmentSpec, SlAwareL1Watcher};
use crate::util::{find_l1_block_by_interop_root_id, find_l1_execute_block_by_batch_number};
use crate::watcher::L1WatcherError;
use crate::{L1WatcherConfig, ProcessRawEvents};

/// Standard L2 bridgehub address — present at the same well-known address on every Gateway.
const L2_BRIDGEHUB_ADDRESS: Address = address!("0x0000000000000000000000000000000000010002");

/// Watches interop root updates emitted by Gateway settlement layers and feeds them into the
/// interop subpool.
///
/// This component reads `NewInteropRoot` events from the Gateway bridgehub's message-root
/// contract, de-duplicates multiple logs for the same `logId`, and inserts the latest
/// `IndexedInteropRoot` into `InteropRootsSubpool`. Per the chain's current invariant, only
/// Gateway-settlement intervals emit interop roots — L1 intervals are skipped wholesale.
///
/// To support a chain that has migrated GW → L1 (or GW → L1 → GW → …), the watcher walks every
/// Gateway interval — historical and active — via [`SlAwareL1Watcher`]. Each historical Gateway
/// segment is bounded by the L1/SL block where the chain executed its last GW batch (after that
/// block the chain is no longer active on that Gateway, so no more interop roots can be relevant
/// to it). The active interval (if Gateway) tails live.
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    interop_roots_subpool: InteropRootsSubpool,
}

impl InteropWatcher {
    /// Builds the watcher iff the chain has at least one Gateway interval. Returns `Ok(None)`
    /// for chains that have only ever settled on L1 — those have no interop roots to watch.
    pub async fn create_watcher(
        intervals: SettlementLayerIntervals,
        gateway_provider: Option<DynProvider>,
        config: L1WatcherConfig,
        l2_chain_id: u64,
        starting_interop_root_id: u64,
        interop_roots_subpool: InteropRootsSubpool,
    ) -> anyhow::Result<Option<SlAwareL1Watcher>> {
        let mut segments = Vec::new();
        for interval in intervals.intervals() {
            // L1 intervals never emit interop roots; skip them outright.
            let IntervalSettlementLayer::Gateway(_) = interval.settlement_layer else {
                continue;
            };
            // Empty intervals are possible when a migration closes without committing anything
            // (`first_batch > last_batch`). Nothing to scan.
            if interval
                .last_batch
                .is_some_and(|lb| interval.first_batch > lb)
            {
                continue;
            }

            let gw_zk_chain = intervals.resolve_proxy(interval.first_batch)?.clone();
            let provider = gw_zk_chain.provider().clone();
            // The bridgehub on every Gateway lives at the well-known L2 bridgehub address.
            // `gateway_provider` is currently equivalent to `gw_zk_chain.provider()` (one
            // configured GW), but threading it through explicitly keeps the design honest if
            // multi-GW history is ever supported.
            let bridgehub_provider = match &gateway_provider {
                Some(p) => p.clone(),
                None => provider.clone(),
            };
            let bridgehub = Bridgehub::new(L2_BRIDGEHUB_ADDRESS, bridgehub_provider, l2_chain_id);
            let message_root = bridgehub.message_root_address().await.with_context(|| {
                format!("failed to fetch message_root address for interval {interval}")
            })?;

            // The chain's `interop_root_id` cursor carries over across migrations, so the same
            // value is the floor anchor on every segment — `find_l1_block_by_interop_root_id`
            // resolves it to the correct block on whichever SL we point it at.
            let start_block = find_l1_block_by_interop_root_id(
                bridgehub.clone(),
                starting_interop_root_id,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to find {} block for interop_root_id={starting_interop_root_id} \
                     in interval {interval}",
                    interval.settlement_layer
                )
            })?;
            let end_block = match interval.last_batch {
                Some(last_batch) => Some(
                    find_l1_execute_block_by_batch_number(gw_zk_chain.clone(), last_batch)
                        .await
                        .with_context(|| {
                            format!(
                                "failed to find Gateway execute block for batch #{last_batch} \
                                 in interval {interval}"
                            )
                        })?,
                ),
                None => None,
            };

            tracing::info!(
                ?interval,
                message_root = ?message_root,
                start_block,
                ?end_block,
                "scheduling interop watcher segment"
            );
            segments.push(SegmentSpec {
                provider,
                address: message_root.into(),
                start_block,
                end_block,
            });
        }
        if segments.is_empty() {
            tracing::info!("chain has no Gateway intervals; skipping interop roots watcher");
            return Ok(None);
        }

        let processor = Self {
            starting_interop_root_id,
            interop_roots_subpool,
        };
        SlAwareL1Watcher::new(config, segments, Box::new(processor)).map(Some)
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
        _provider: &DynProvider,
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
            sides: event.sides.clone(),
        };

        self.interop_roots_subpool
            .add_root(IndexedInteropRoot {
                log_id,
                root: interop_root,
            })
            .await;
        Ok(())
    }
}
