use crate::watcher::{L1Watcher, L1WatcherError, WatchedEvent, WatchedEventTryFrom};
use crate::{L1WatcherConfig};
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use zksync_os_types::{InteropRoot, InteropRootPosition};
use std::convert::Infallible;
use std::time::Duration;
use alloy::rpc::types::Log;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub struct L1InteropRootWatcher {
    l1_watcher: L1Watcher<InteropRoot>,
    poll_interval: Duration,
    output: mpsc::Sender<InteropRoot>,
    l2_chain_id: u64,
    next_interop_root_pos: InteropRootPosition,
}

impl L1InteropRootWatcher {
    pub async fn new(
        config: L1WatcherConfig,
        provider: DynProvider,
        message_root_address: Address,
        output: mpsc::Sender<InteropRoot>,
        l2_chain_id: u64,
        next_interop_root_pos: InteropRootPosition,
    ) -> anyhow::Result<Self> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            ?message_root_address,
            "initializing L1 interop root watcher"
        );

        let l1_watcher = L1Watcher::new(
            provider,
            message_root_address,
            next_interop_root_pos.sl_block_number,
            config.max_blocks_to_process,
        );

        Ok(Self {
            l1_watcher,
            poll_interval: config.poll_interval,
            output,
            l2_chain_id,
            next_interop_root_pos
        })
    }

    async fn poll(&mut self) -> L1InteropRootWatcherResult<()> {
        let interop_roots = self.l1_watcher.poll().await?;

        let l2_chain_id = U256::from(self.l2_chain_id);
        for interop_root in interop_roots.into_iter().filter(|root| root.chain_id != l2_chain_id) {
            if interop_root.pos < self.next_interop_root_pos {
                tracing::debug!(
                    interop_root_pos = ?interop_root.pos,
                    "skipping already processed interop root",
                )
            } else {
                self.next_interop_root_pos = InteropRootPosition {
                    sl_block_number: interop_root.pos.sl_block_number,
                    log_index_in_block: interop_root.pos.log_index_in_block + 1,
                };
                tracing::debug!(
                    interop_root_pos = ?interop_root.pos,
                    "sending new interop root",
                );
                self.output
                    .send(interop_root)
                    .await
                    .map_err(|_| L1InteropRootWatcherError::OutputClosed)?;
            }

        }

        Ok(())
    }

    pub async fn run(mut self) -> L1InteropRootWatcherResult<()> {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }
}

impl WatchedEventTryFrom<NewInteropRoot> for InteropRoot {
    type Error = Infallible;

    fn try_from(value: (NewInteropRoot, Log)) -> Result<Self, Self::Error> {
        Ok(InteropRoot::from(value))
    }
}

impl WatchedEvent for InteropRoot {
    const NAME: &'static str = "new_interop_root";

    type SolEvent = NewInteropRoot;
}

pub type L1InteropRootWatcherResult<T> = Result<T, L1InteropRootWatcherError>;
pub type L1InteropRootWatcherError = L1WatcherError<Infallible>;
