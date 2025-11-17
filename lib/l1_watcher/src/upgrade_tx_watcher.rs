use std::sync::Arc;

use crate::factory_deps::load_factory_deps;
use crate::watcher::{L1Watcher, L1WatcherError, ProcessL1Event};
use crate::{L1WatcherConfig, util};
use alloy::dyn_abi::SolType;
use alloy::primitives::{Address, B256, BlockNumber, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IChainAdmin::UpdateUpgradeTimestamp;
use zksync_os_contract_interface::IChainTypeManager::{NewUpgradeCutData, ProposedUpgrade};
use zksync_os_contract_interface::ZkChain;
use zksync_os_types::{
    L1UpgradeEnvelope, ProtocolSemanticVersion, ProtocolSemanticVersionError, UpgradeTransaction,
};

// TODO: disabled until bytecode supplier integration is ready
// use zksync_os_contract_interface::IBytecodeSupplier::BytecodePublished;
// use zk_os_api::helpers::set_properties_code;
// use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;

/// Limit the number of L1 blocks to scan when looking for the set timestamp transaction.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;
/// The constant value is higher than for other watchers, since we're looking for rare/specific events
/// and we don't expect a lot of results.
const UPGRADE_DATA_LOOKBEHIND_BLOCKS: u64 = 2_500_000;

pub struct L1UpgradeTxWatcher {
    provider: DynProvider,
    /// Address of the bytecode supplier contract (used to detect published bytecode preimages)
    #[allow(dead_code)] // TODO: enable once bytecode supplier integration is ready
    bytecode_supplier_address: Address,
    /// Address of the CTM contract (used to detect upgrade priority transactions)
    ctm: Address,
    current_protocol_version: ProtocolSemanticVersion,
    output: mpsc::Sender<UpgradeTransaction>,

    // Needed to process L1 blocks in chunks.
    max_blocks_to_process: u64,
}

impl L1UpgradeTxWatcher {
    pub async fn new(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        bytecode_supplier_address: Address,
        current_protocol_version: ProtocolSemanticVersion,
        output: mpsc::Sender<UpgradeTransaction>,
    ) -> anyhow::Result<L1Watcher<Self>> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing upgrade transaction watcher"
        );

        let admin = zk_chain.get_admin().await?;
        tracing::info!(admin = ?admin, "resolved chain admin");

        let ctm = zk_chain.get_chain_type_manager().await?;
        tracing::info!(ctm = ?ctm, "resolved chain type manager");

        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_l1_block = find_l1_block_by_protocol_version(zk_chain.clone(), current_protocol_version.clone())
            .await
            .or_else(|err| {
                // This may error on Anvil with `--load-state` - as it doesn't support `eth_call` even for recent blocks.
                // We default to `0` in this case - `eth_getLogs` are still supported.
                // Assert that we don't fallback on longer chains (e.g. Sepolia)
                if current_l1_block > INITIAL_LOOKBEHIND_BLOCKS {
                    anyhow::bail!(
                        "Binary search failed with {err}. Cannot default starting block to zero for a long chain. Current L1 block number: {current_l1_block}. Limit: {INITIAL_LOOKBEHIND_BLOCKS}."
                    );
                } else {
                    Ok(0)
                }
            })?;
        // Right now, bytecodes supplied address is provided as a configuration, since it's not discoverable from L1
        // Sanity check: make sure that the value provided for this config is correct.
        anyhow::ensure!(
            !zk_chain
                .provider()
                .get_code_at(bytecode_supplier_address)
                .await?
                .is_empty(),
            "Bytecode supplier contract is not deployed at expected address {bytecode_supplier_address:?}"
        );

        tracing::info!(last_l1_block, "checking block starting from");

        let this = Self {
            provider: zk_chain.provider().clone(),
            bytecode_supplier_address,
            ctm,
            current_protocol_version,
            output,
            max_blocks_to_process: config.max_blocks_to_process,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            admin,
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this,
        );

        Ok(l1_watcher)
    }

    async fn fetch_upgrade_tx(
        &self,
        request: &L1UpgradeRequest,
    ) -> anyhow::Result<UpgradeTransaction> {
        let L1UpgradeRequest {
            timestamp,
            protocol_version,
            raw_protocol_version,
        } = request;

        // TODO: for now we assume that upgrades cannot be skipped, e.g.
        // each chain upgrades before the new upgrade is published.
        // This is a temporary solution and should be fixed ASAP.
        let mut current_block = self.provider.get_block_number().await?;
        let start_block = current_block
            .saturating_sub(UPGRADE_DATA_LOOKBEHIND_BLOCKS) // Upgrade could've been set a long time ago.
            .max(1u64);

        // TODO: upgrade data can be much farther in history and we can't easily find a block where it was set,
        // so we scan linearly (in order to not go over the limit per request) but move backwards since it's
        // more likely to be recent.
        let mut upgrade_cut_data_logs = Vec::new();
        while current_block >= start_block && upgrade_cut_data_logs.is_empty() {
            let from_block = current_block
                .saturating_sub(self.max_blocks_to_process - 1)
                .max(start_block);

            let filter = Filter::new()
                .from_block(from_block)
                .to_block(current_block)
                .address(self.ctm)
                .event_signature(NewUpgradeCutData::SIGNATURE_HASH)
                .topic1(*raw_protocol_version);
            upgrade_cut_data_logs = self.provider.get_logs(&filter).await?;
            current_block = from_block.saturating_sub(1);
        }

        if upgrade_cut_data_logs.is_empty() {
            anyhow::bail!(
                "no upgrade cut found for the suggested protocol version: {}",
                protocol_version
            );
        }
        if upgrade_cut_data_logs.len() > 1 {
            anyhow::bail!(
                "multiple upgrade cuts found for the suggested protocol version: {}",
                protocol_version
            );
        }
        let raw_diamond_cut: Log<NewUpgradeCutData> =
            upgrade_cut_data_logs[0].log_decode().unwrap();
        let diamond_cut_data = raw_diamond_cut.inner.data.diamondCutData;
        let proposed_upgrade =
            ProposedUpgrade::abi_decode(&diamond_cut_data.initCalldata[4..]).unwrap(); // TODO: we're in fact parsing `upgrade(..)` signature here

        let patch_only = protocol_version.minor == self.current_protocol_version.minor;
        let (l2_upgrade_tx, force_preimages) = if patch_only {
            (None, Vec::new())
        } else {
            let tx = L1UpgradeEnvelope::try_from(proposed_upgrade.l2ProtocolUpgradeTx).unwrap();
            let force_preimages = self.fetch_force_preimages(&tx.inner.factory_deps).await?;

            tracing::info!(
                "Fetched {} preimages from the hardcoded file.",
                force_preimages.len()
            );
            (Some(tx), force_preimages)
        };

        let upgrade_tx = UpgradeTransaction {
            tx: l2_upgrade_tx,
            timestamp: *timestamp,
            protocol_version: protocol_version.clone(),
            force_preimages,
        };

        Ok(upgrade_tx)
    }

    async fn wait_until_timestamp(&self, target_timestamp: u64) {
        let mut current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before UNIX_EPOCH")
            .as_secs();
        while current_timestamp < target_timestamp {
            let wait_duration =
                std::time::Duration::from_secs(target_timestamp - current_timestamp);
            tracing::info!(
                wait_duration = ?wait_duration,
                target_timestamp = target_timestamp,
                "waiting until the upgrade timestamp to send the upgrade transaction"
            );
            tokio::time::sleep(wait_duration).await;
            current_timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time before UNIX EPOCH")
                .as_secs();
        }
    }

    async fn fetch_force_preimages(
        &self,
        _hashes: &[B256],
    ) -> anyhow::Result<Vec<(B256, Vec<u8>)>> {
        // HACK: For now, we load preimages from a hardcoded JSON file.
        load_factory_deps()

        // // TODO: Bytecode supplier is not ready yet for ZKsync OS.
        // panic!("fetching force deployment preimages is not yet implemented");

        // tracing::info!(
        //     num_hashes = hashes.len(),
        //     "fetching force deployment preimages from bytecode supplier"
        // );

        // // TODO: for now we assume that bytecodes are published within lookbehind range
        // let current_block = self.provider.get_block_number().await?;
        // let start_block = current_block
        //     .saturating_sub(MAX_L1_BLOCKS_LOOKBEHIND)
        //     .max(1u64);

        // let mut preimages = Vec::new();
        // for hash in hashes {
        //     let filter = Filter::new()
        //         .from_block(start_block)
        //         .to_block(current_block)
        //         .address(self.bytecode_supplier_address)
        //         .event_signature(BytecodePublished::SIGNATURE_HASH)
        //         .topic1(*hash);
        //     let logs = self.provider.get_logs(&filter).await?;
        //     anyhow::ensure!(
        //         logs.len() == 1,
        //         "expected exactly one log for bytecode hash {hash:?}, got {logs:?}"
        //     );
        //     let sol_event = BytecodePublished::decode_log(&logs[0].inner)?.data;

        //     // NOTE: it is guaranteed that `bytecodeHashes` from the transaction correspond to
        //     // the `BytecodePublished` events, but there is no guarantee that the bytecode hash
        //     // the server expects is the same as the one used in the event. So, we need to re-calculate
        //     // the hash here.
        //     let mut account_properties = AccountProperties::default();
        //     set_properties_code(&mut account_properties, &sol_event.bytecode);
        //     let calculated_hash = B256::from_slice(account_properties.bytecode_hash.as_u8_ref());

        //     preimages.push((calculated_hash, sol_event.bytecode.to_vec()));
        // }
        // Ok(preimages)
    }
}

impl ProcessL1Event for L1UpgradeTxWatcher {
    const NAME: &'static str = "upgrade_txs";

    type SolEvent = UpdateUpgradeTimestamp;
    type WatchedEvent = L1UpgradeRequest;
    type Error = UpgradeTxWatcherError;

    async fn process_event(
        &mut self,
        request: L1UpgradeRequest,
    ) -> Result<(), L1WatcherError<Self::Error>> {
        if request.protocol_version <= self.current_protocol_version {
            tracing::info!(
                ?request.protocol_version,
                ?self.current_protocol_version,
                "ignoring upgrade timestamp for older or equal protocol version"
            );
            return Ok(());
        }

        // In localhost environment, we may want to test upgrades to non-live versions, but
        // we don't want to allow them anywhere else.
        if !request.protocol_version.is_live() {
            tracing::warn!(
                ?request.protocol_version,
                "received a protocol version that is not marked as live"
            );
            // Only allow non-live versions in localhost environment.
            if self.provider.get_chain_id().await? != 31337 {
                panic!(
                    "Received an upgrade to a non-live protocol version: {:?}",
                    request.protocol_version
                );
            }
        }

        let upgrade_tx = self
            .fetch_upgrade_tx(&request)
            .await
            .map_err(L1WatcherError::from)?;

        tracing::info!(
            protocol_version = ?upgrade_tx.protocol_version,
            target_timestamp = request.timestamp,
            "detected upgrade transaction to be sent"
        );

        // Wait until the timestamp before sending the upgrade tx, so that it's immediately executable.
        // TODO: this will block the watcher, so if e.g. a timestamp is set far in the future, and then an event
        // to override it is emitted, we will not be able to process it.
        self.wait_until_timestamp(request.timestamp).await;

        tracing::info!(
            protocol_version = ?upgrade_tx.protocol_version,
            "sending upgrade transaction to the mempool"
        );

        self.output.send(upgrade_tx.clone()).await.map_err(|e| {
            L1WatcherError::Batch(anyhow::anyhow!("failed to send upgrade tx: {}", e))
        })?;

        self.current_protocol_version = upgrade_tx.protocol_version;

        Ok(())
    }
}

/// Request for the server to upgrade at a certain timestamp.
/// Parsed from `UpdateUpgradeTimestamp` L1 event.
#[derive(Debug, Clone)]
pub struct L1UpgradeRequest {
    raw_protocol_version: U256,
    protocol_version: ProtocolSemanticVersion,
    /// Timestamp in seconds since UNIX_EPOCH
    timestamp: u64,
}

impl TryFrom<UpdateUpgradeTimestamp> for L1UpgradeRequest {
    type Error = UpgradeTxWatcherError;

    fn try_from(event: UpdateUpgradeTimestamp) -> Result<Self, Self::Error> {
        let protocol_version = ProtocolSemanticVersion::try_from(event.protocolVersion)?;

        let timestamp_u64 = u64::try_from(event.upgradeTimestamp)
            .map_err(|_| UpgradeTxWatcherError::TimestampExceedsU64(event.upgradeTimestamp))?;

        Ok(Self {
            raw_protocol_version: event.protocolVersion,
            protocol_version,
            timestamp: timestamp_u64,
        })
    }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum UpgradeTxWatcherError {
    #[error("Timestamp exceeds u64: {0}")]
    TimestampExceedsU64(U256),
    #[error("Incorrect protocol version: {0}")]
    IncorrectProtocolVersion(#[from] ProtocolSemanticVersionError),
}

async fn find_l1_block_by_protocol_version(
    zk_chain: ZkChain<DynProvider>,
    protocol_version: ProtocolSemanticVersion,
) -> anyhow::Result<BlockNumber> {
    let protocol_version = protocol_version.packed()?;

    util::find_l1_block_by_predicate(Arc::new(zk_chain), move |zk, block| async move {
        let res = zk.get_raw_protocol_version(block.into()).await?;
        Ok(res >= protocol_version)
    })
    .await
}
