use std::collections::HashMap;

use alloy::primitives::ruint::FromUintError;
use alloy::rpc::types::{Log, Topic, ValueOrArray};
use alloy::sol_types::SolEvent;
use alloy::{primitives::Address, providers::DynProvider};
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot};
use zksync_os_mempool::subpools::interop_roots::InteropRootsSubpool;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents};

pub struct InteropWatcher {
    contract_address: Address,
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
            contract_address,
            starting_interop_root_id,
            interop_roots_subpool,
        };

        let l1_watcher = L1Watcher::new(
            bridgehub.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.confirmations,
            l1_chain_id,
            config.poll_interval,
            Box::new(this),
        )
        .await?;

        Ok(l1_watcher)
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

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        self.contract_address.into()
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

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
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
