use alloy::primitives::ruint::FromUintError;
use alloy::providers::DynProvider;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use std::collections::HashMap;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::InteropRoot;
use zksync_os_mempool::subpools::interop_roots::InteropRootsSubpool;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents};

/// Watches interop root updates on the settlement layer and feeds them into the interop subpool.
///
/// This component reads `NewInteropRoot` events from the bridgehub message root contract,
/// de-duplicates multiple logs for the same `logId`, and inserts the latest `IndexedInteropRoot`
/// into `InteropRootsSubpool`.
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    interop_roots_subpool: InteropRootsSubpool,
}

impl InteropWatcher {
    pub async fn create_watcher(
        bridgehub: Bridgehub<DynProvider>,
        config: L1WatcherConfig,
        starting_interop_root_id: u64,
        interop_roots_subpool: InteropRootsSubpool,
        l1_chain_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        let contract_address = bridgehub.message_root_address().await?;

        tracing::info!(
            contract_address = ?contract_address,
            starting_interop_root_id,
            "initializing interop watcher"
        );

        let next_l1_block =
            find_l1_block_by_interop_root_id(bridgehub.clone(), starting_interop_root_id).await?;

        let this = Self {
            starting_interop_root_id,
            interop_roots_subpool,
        };

        L1Watcher::new(
            config,
            bridgehub.provider().clone(),
            contract_address.into(),
            next_l1_block,
            None,
            l1_chain_id,
            Box::new(this),
        )
        .await
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
