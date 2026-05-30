use std::time::{Duration, Instant};

use alloy::eips::BlockId;
use alloy::providers::Provider;
use anyhow::Context as _;
use zksync_os_alloy_ext::network::Zksync;

/// Helper trait to implement additional functionality for tests on top of ZKsync provider.
#[allow(async_fn_in_trait)]
pub trait ZksyncTestingProvider: Provider<Zksync> {
    /// Will wait until the given block is finalized.
    /// This method can hang if the specified block is never produced, so it's recommended
    /// to use `wait_finalized_with_timeout` instead.
    async fn wait_finalized(&self, block_number: u64) -> anyhow::Result<()> {
        tracing::info!("Waiting for block {block_number} to be finalized on L1");
        loop {
            let finalized_block = self.get_block_number_by_id(BlockId::finalized()).await?;
            if finalized_block >= Some(block_number) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Will wait until the given block is finalized, or timeout occurs.
    async fn wait_finalized_with_timeout(
        &self,
        block_number: u64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(timeout, self.wait_finalized(block_number))
            .await
            .with_context(|| format!("Block {block_number} was not finalized on L1"))??;
        Ok(())
    }

    /// Will make sure block is NOT finalized even after timeout
    async fn wait_not_finalized(&self, block_number: u64, timeout: Duration) -> anyhow::Result<()> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let finalized_block = self.get_block_number_by_id(BlockId::finalized()).await?;
            if finalized_block >= Some(block_number) {
                return Err(anyhow::anyhow!("Block {block_number} was finalized on L1"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    async fn wait_for_block(&self, block_number: u64) -> anyhow::Result<()> {
        tracing::info!("Waiting for block {block_number} to be processed on L2");
        loop {
            let latest_block = self.get_block_number_by_id(BlockId::latest()).await?;
            if latest_block >= Some(block_number) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl<P> ZksyncTestingProvider for P where P: Provider<Zksync> {}
