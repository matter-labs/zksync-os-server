use crate::watcher::{L1Watcher, L1WatcherError, WatchedEvent};
use crate::{L1WatcherConfig, util};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::sol_types::SolEvent;
use zksync_os_types::{InteropRoot, InteropRootError};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{IBridgehub, IZKChain, ZkChain};
use zksync_os_storage_api::{ReadBatch, WriteFinality};

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub type L1InteropRootWatcherResult<T> = Result<T, L1InteropRootWatcherError>;
pub type L1InteropRootWatcherError = L1WatcherError<InteropRootError>;

impl WatchedEvent for InteropRoot {
    const NAME: &'static str = "new_interop_root";

    type SolEvent = NewInteropRoot;
}

pub struct L1InteropRootWatcher {
    l1_watcher: L1Watcher<InteropRoot>,
    poll_interval: Duration,
    output: mpsc::Sender<InteropRoot>,
}

impl L1InteropRootWatcher {
    pub async fn new(
        config: L1WatcherConfig,
        provider: DynProvider,
        zk_chain_address: Address,
        output: mpsc::Sender<InteropRoot>,
    ) -> anyhow::Result<Self> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            ?zk_chain_address,
            "initializing L1 interop root watcher"
        );

        let bridgehub = IZKChain::new(zk_chain_address, &provider).getBridgehub().call().await.unwrap();
        let message_root = IBridgehub::new(bridgehub, &provider).messageRoot().call().await.unwrap();

        let current_l1_block = provider.get_block_number().await?;

        let l1_watcher = L1Watcher::new(
            provider,
            message_root,
            current_l1_block,
            config.max_blocks_to_process,
        );

        tracing::info!(
            ?message_root, 
            "watching message root contract"
        );

        Ok(Self {
            l1_watcher,
            poll_interval: config.poll_interval,
            output,
        })
    }

    async fn poll(&mut self) -> L1InteropRootWatcherResult<()> {
        let interop_roots = self.l1_watcher.poll().await?;
        
        for interop_root in interop_roots {
            self.output
                .send(interop_root)
                .await
                .map_err(|_| L1InteropRootWatcherError::OutputClosed)?;
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
