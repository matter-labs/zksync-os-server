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
    pub finalized_block: BlockNumber,
}

pub fn run(
    provider: DynProvider,
    runtime: &Runtime,
    task_name: &'static str,
    poll_interval: Duration,
) -> watch::Receiver<ChainHead> {
    let (l1_head, receiver) = watch::channel(ChainHead::default());
    runtime.spawn_critical_task(task_name, async move {
        let mut timer = tokio::time::interval(poll_interval);
        loop {
            timer.tick().await;
            if let Err(e) = poll(&provider, &l1_head).await {
                tracing::error!("watcher cache fatal error: {e}");
                panic!("watcher cache failed: {e}");
            }
        }
    });
    receiver
}

async fn poll(
    provider: &DynProvider,
    l1_head: &watch::Sender<ChainHead>,
) -> alloy::transports::TransportResult<()> {
    let latest_block = provider.get_block_number().await?;
    let finalized_block = provider
        .get_block_number_by_id(BlockId::finalized())
        .await?
        .expect("The chain does not have any finalized blocks yet.");
    let next = ChainHead {
        latest_block,
        finalized_block,
    };
    l1_head.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
    Ok(())
}

impl ChainHead {
    pub(crate) fn get_block_number(&self, boundary: BlockBoundary) -> BlockNumber {
        match boundary {
            BlockBoundary::Confirmed { confirmations } => {
                self.latest_block.saturating_sub(confirmations)
            }
            BlockBoundary::Finalized => self.finalized_block,
        }
    }
}
