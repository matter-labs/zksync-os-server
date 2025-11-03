use crate::watcher::{L1Watcher, L1WatcherError, ProcessL1Event};
use crate::{L1WatcherConfig, util};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use std::sync::Arc;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::ZkChain;
use zksync_os_types::{L1EnvelopeError, L1PriorityEnvelope};

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub struct L1UpgradeTxWatcher {
    /// Address of the chain admin contract (used to detect suggested upgrade timestamps)
    admin: Address,
    /// Address of the CTM contract (used to detect upgrade priority transactions)
    ctm: Address,
    output: mpsc::Sender<L1PriorityEnvelope>,
}

impl L1UpgradeTxWatcher {
    pub async fn new(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        output: mpsc::Sender<L1PriorityEnvelope>,
    ) -> anyhow::Result<L1Watcher<Self>> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 transaction watcher"
        );

        let admin = zk_chain.get_admin().await?;
        tracing::info!(admin = ?admin, "resolved chain admin");

        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let next_l1_block = 1; // TODO: check the contract for the upgrade info and compare it to last known version

        tracing::info!(next_l1_block, "resolved on L1");

        todo!();

        // let this = Self { output };
        // let l1_watcher = L1Watcher::new(
        //     zk_chain.provider().clone(),
        //     *zk_chain.address(),
        //     next_l1_block,
        //     config.max_blocks_to_process,
        //     config.poll_interval,
        //     this,
        // );

        // Ok(l1_watcher)
    }
}

impl ProcessL1Event for L1UpgradeTxWatcher {
    const NAME: &'static str = "upgrade_txs";

    type SolEvent = NewPriorityRequest;
    type WatchedEvent = L1PriorityEnvelope;
    type Error = L1EnvelopeError;

    async fn process_event(
        &mut self,
        tx: L1PriorityEnvelope,
    ) -> Result<(), L1WatcherError<Self::Error>> {
        todo!();
        Ok(())
    }
}
