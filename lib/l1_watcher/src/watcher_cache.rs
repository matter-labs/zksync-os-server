use alloy::eips::BlockId;
use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use reth_tasks::Runtime;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockBoundary {
    Confirmed { confirmations: BlockNumber },
    Finalized,
}

/// Used to track changes & notify watchers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChainHead {
    pub latest_block: BlockNumber,
    pub finalized_block: Option<BlockNumber>,
}

/// Used for reading L1 data which might be used by more than one watcher.
/// Currently only block numbers.
#[derive(Clone, Debug)]
pub struct WatcherCache {
    provider: DynProvider,
    l1_head: watch::Sender<ChainHead>,
}

impl WatcherCache {
    pub fn new(provider: DynProvider) -> Self {
        let (l1_head, _) = watch::channel(ChainHead::default());
        Self { provider, l1_head }
    }

    pub(crate) fn provider(&self) -> &DynProvider {
        &self.provider
    }

    pub fn subscribe(&self) -> watch::Receiver<ChainHead> {
        self.l1_head.subscribe()
    }

    pub fn get_block_number(&self, boundary: BlockBoundary) -> Option<BlockNumber> {
        self.l1_head.borrow().get_block_number(boundary)
    }

    pub fn run(&self, runtime: &Runtime, task_name: &'static str, poll_interval: Duration) {
        let this = self.clone();
        runtime.spawn_critical_task(task_name, async move {
            this.run_inner(poll_interval).await;
        });
    }

    async fn run_inner(self, poll_interval: Duration) {
        let mut timer = tokio::time::interval(poll_interval);
        loop {
            timer.tick().await;
            if let Err(e) = self.poll().await {
                tracing::error!("watcher cache fatal error: {e}");
                panic!("watcher cache failed: {e}");
            }
        }
    }

    async fn poll(&self) -> alloy::transports::TransportResult<()> {
        let latest_block = self.provider.get_block_number().await?;
        let finalized_block = self
            .provider
            .get_block_number_by_id(BlockId::finalized())
            .await?;
        let next = ChainHead {
            latest_block,
            finalized_block,
        };
        self.l1_head.send_if_modified(|current| {
            if *current == next {
                false
            } else {
                *current = next;
                true
            }
        });
        Ok(())
    }
}

impl ChainHead {
    fn get_block_number(&self, boundary: BlockBoundary) -> Option<BlockNumber> {
        match boundary {
            BlockBoundary::Confirmed { confirmations } => {
                Some(self.latest_block.saturating_sub(confirmations))
            }
            BlockBoundary::Finalized => self.finalized_block,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_boundary_saturates() {
        let state = ChainHead {
            latest_block: 7,
            finalized_block: Some(3),
        };

        assert_eq!(
            state.get_block_number(BlockBoundary::Confirmed { confirmations: 10 }),
            Some(0)
        );
        assert_eq!(
            state.get_block_number(BlockBoundary::Confirmed { confirmations: 2 }),
            Some(5)
        );
    }

    #[test]
    fn finalized_boundary_can_be_missing() {
        let state = ChainHead {
            latest_block: 7,
            finalized_block: None,
        };

        assert_eq!(state.get_block_number(BlockBoundary::Finalized), None);
    }

    #[tokio::test]
    async fn subscribers_wake_only_when_state_changes() {
        let (sender, mut receiver) = watch::channel(ChainHead::default());

        sender.send_if_modified(|current| {
            *current = ChainHead::default();
            false
        });
        assert!(receiver.has_changed().is_ok_and(|changed| !changed));

        sender.send_if_modified(|current| {
            *current = ChainHead {
                latest_block: 1,
                finalized_block: None,
            };
            true
        });
        receiver.changed().await.unwrap();
        assert_eq!(receiver.borrow().latest_block, 1);
    }
}
