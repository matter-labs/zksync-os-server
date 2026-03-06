use std::collections::HashMap;
use std::sync::Arc;

use crate::util::ANVIL_L1_CHAIN_ID;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::dyn_abi::SolType;
use alloy::primitives::{Address, B256, BlockNumber, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use blake2::{Blake2s256, Digest as BlakeDigest};
use zksync_os_contract_interface::IChainAdmin::UpdateUpgradeTimestamp;
use zksync_os_contract_interface::IChainTypeManager::{NewUpgradeCutData, ProposedUpgrade};
use zksync_os_contract_interface::ZkChain;
use zksync_os_mempool::subpools::upgrade::UpgradeSubpool;
use zksync_os_types::{
    L1UpgradeEnvelope, ProtocolSemanticVersion, ProtocolSemanticVersionError, UpgradeInfo,
    UpgradeMetadata,
};

alloy::sol! {
    #[derive(Debug)]
    event EVMBytecodePublished(bytes32 indexed bytecodeHash, bytes bytecode);

    #[sol(rpc)]
    interface IChainTypeManagerBytecodeSupplier {
        function L1_BYTECODES_SUPPLIER() external view returns (address);
    }
}

/// Limit the number of L1 blocks to scan when looking for the set timestamp transaction.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;
/// The constant value is higher than for other watchers, since we're looking for rare/specific events
/// and we don't expect a lot of results.
const UPGRADE_DATA_LOOKBEHIND_BLOCKS: u64 = 2_500_000;

pub struct L1UpgradeTxWatcher {
    admin_contract_l1: Address,

    provider_l1: DynProvider,
    provider_sl: DynProvider,
    /// Address of the bytecode supplier contract (used to detect published bytecode preimages)
    bytecode_supplier_address: Address,
    /// Address of the CTM contract (used to detect upgrade priority transactions)
    ctm_sl: Address,
    current_protocol_version: ProtocolSemanticVersion,
    upgrade_subpool: UpgradeSubpool,

    // Needed to process L1 blocks in chunks.
    max_blocks_to_process: u64,
}

impl L1UpgradeTxWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain_l1: ZkChain<DynProvider>,
        zk_chain_sl: ZkChain<DynProvider>,
        bytecode_supplier_address: Address,
        current_protocol_version: ProtocolSemanticVersion,
        upgrade_subpool: UpgradeSubpool,
    ) -> anyhow::Result<L1Watcher> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address_l1 = ?zk_chain_l1.address(),
            zk_chain_address_sl = ?zk_chain_sl.address(),
            "initializing upgrade transaction watcher"
        );

        let admin_l1 = zk_chain_l1.get_admin().await?;
        tracing::info!(admin_l1 = ?admin_l1, "resolved chain admin");

        let ctm_sl = zk_chain_sl.get_chain_type_manager().await?;
        tracing::info!(ctm_sl = ?ctm_sl, "resolved chain type manager");

        let current_l1_block = zk_chain_l1.provider().get_block_number().await?;
        let last_l1_block = find_l1_block_by_protocol_version(zk_chain_l1.clone(), current_protocol_version.clone())
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
            !zk_chain_l1
                .provider()
                .get_code_at(bytecode_supplier_address)
                .await?
                .is_empty(),
            "Bytecode supplier contract is not deployed at expected address {bytecode_supplier_address:?}"
        );

        tracing::info!(last_l1_block, "checking block starting from");

        let this = Self {
            admin_contract_l1: admin_l1,
            provider_l1: zk_chain_l1.provider().clone(),
            provider_sl: zk_chain_sl.provider().clone(),
            bytecode_supplier_address,
            ctm_sl,
            current_protocol_version,
            upgrade_subpool,
            max_blocks_to_process: config.max_blocks_to_process,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain_l1.provider().clone(),
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );

        Ok(l1_watcher)
    }

    async fn fetch_upgrade_info(&self, request: &L1UpgradeRequest) -> anyhow::Result<UpgradeInfo> {
        let L1UpgradeRequest {
            timestamp,
            protocol_version,
            raw_protocol_version,
        } = request;

        // TODO: for now we assume that upgrades cannot be skipped, e.g.
        // each chain upgrades before the new upgrade is published.
        // This is a temporary solution and should be fixed ASAP.
        let mut current_block = self.provider_sl.get_block_number().await?;
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
                .address(self.ctm_sl)
                .event_signature(NewUpgradeCutData::SIGNATURE_HASH)
                .topic1(*raw_protocol_version);
            upgrade_cut_data_logs = self.provider_sl.get_logs(&filter).await?;
            current_block = from_block.saturating_sub(1);
        }

        if upgrade_cut_data_logs.is_empty() {
            anyhow::bail!(
                "no upgrade cut found for the suggested protocol version: {}",
                protocol_version
            );
        }
        if upgrade_cut_data_logs.len() > 1 {
            tracing::warn!(
                %protocol_version,
                "multiple upgrade cuts found for the suggested protocol version; picking the most recent one"
            );
        }
        // Safe unwrap because of checks above
        // `last()` because, even though we scan backwards, each scan returns a list of ascending result
        let upgrade_cut_data = upgrade_cut_data_logs.last().unwrap();
        let raw_diamond_cut: Log<NewUpgradeCutData> = upgrade_cut_data.log_decode()?;
        let diamond_cut_data = raw_diamond_cut.inner.data.diamondCutData;
        let proposed_upgrade =
            ProposedUpgrade::abi_decode(&diamond_cut_data.initCalldata[4..]).unwrap(); // TODO: we're in fact parsing `upgrade(..)` signature here

        let patch_only = protocol_version.minor == self.current_protocol_version.minor;
        let (l2_upgrade_tx, force_preimages) = if patch_only {
            (None, Vec::new())
        } else {
            let tx = L1UpgradeEnvelope::try_from(proposed_upgrade.l2ProtocolUpgradeTx).unwrap();
            let force_preimages = self.fetch_force_preimages().await?;

            tracing::info!(
                resolved_preimages = force_preimages.len(),
                "resolved force deployment preimages from bytecode supplier"
            );
            (Some(tx), force_preimages)
        };

        let upgrade_tx = UpgradeInfo {
            tx: l2_upgrade_tx,
            metadata: UpgradeMetadata {
                timestamp: *timestamp,
                protocol_version: protocol_version.clone(),
                force_preimages,
            },
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

    async fn fetch_force_preimages(&self) -> anyhow::Result<Vec<(B256, Vec<u8>)>> {
        let active_supplier = self.resolve_active_bytecode_supplier().await?;

        let mut current_block = self.provider_l1.get_block_number().await?;
        let start_block = current_block
            .saturating_sub(UPGRADE_DATA_LOOKBEHIND_BLOCKS)
            .max(1u64);

        let mut by_hash: HashMap<B256, Vec<u8>> = HashMap::new();

        while current_block >= start_block {
            let from_block = current_block
                .saturating_sub(self.max_blocks_to_process - 1)
                .max(start_block);
            let filter = Filter::new()
                .from_block(from_block)
                .to_block(current_block)
                .address(active_supplier)
                .event_signature(EVMBytecodePublished::SIGNATURE_HASH);
            let logs = self.provider_l1.get_logs(&filter).await?;

            for log in logs {
                let published = EVMBytecodePublished::decode_log(&log.inner)?.data;
                let evm_hash = B256::from(published.bytecodeHash);
                let zkos_hash = zkos_hash_from_bytecode(&published.bytecode);
                let bytecode = published.bytecode.to_vec();

                by_hash.insert(evm_hash, bytecode.clone());
                by_hash.insert(zkos_hash, bytecode);
            }

            current_block = from_block.saturating_sub(1);
        }

        tracing::info!(
            supplier = ?active_supplier,
            num_preimages = by_hash.len(),
            "fetched force deployment preimages from bytecode supplier"
        );

        Ok(by_hash.into_iter().collect())
    }

    async fn resolve_active_bytecode_supplier(&self) -> anyhow::Result<Address> {
        let ctm = IChainTypeManagerBytecodeSupplier::new(self.ctm_sl, self.provider_sl.clone());
        let l1_address =
            ctm.L1_BYTECODES_SUPPLIER().call().await.map_err(|err| {
                anyhow::anyhow!("failed to fetch bytecode supplier from CTM: {err}")
            })?;

        anyhow::ensure!(
            l1_address != Address::ZERO,
            "CTM returned zero address for bytecode supplier"
        );

        if l1_address != self.bytecode_supplier_address {
            tracing::warn!(
                configured_supplier = ?self.bytecode_supplier_address,
                l1_supplier = ?l1_address,
                ctm = ?self.ctm_sl,
                "bytecode supplier address on L1 differs from configured; using L1 address"
            );
        }

        Ok(l1_address)
    }
}

fn zkos_hash_from_bytecode(bytecode: &[u8]) -> B256 {
    // Matches Utils.getZKOSBytecodeInfo -> blake2s256(bytecode)
    let digest = Blake2s256::digest(bytecode);
    B256::from_slice(digest.as_slice())
}

#[async_trait::async_trait]
impl ProcessL1Event for L1UpgradeTxWatcher {
    const NAME: &'static str = "upgrade_txs";

    type SolEvent = UpdateUpgradeTimestamp;
    type WatchedEvent = L1UpgradeRequest;

    fn contract_address(&self) -> Address {
        self.admin_contract_l1
    }

    async fn process_event(
        &mut self,
        request: L1UpgradeRequest,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
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
            if self.provider_l1.get_chain_id().await? != ANVIL_L1_CHAIN_ID {
                panic!(
                    "Received an upgrade to a non-live protocol version: {:?}",
                    request.protocol_version
                );
            }
        }

        let upgrade_info = self
            .fetch_upgrade_info(&request)
            .await
            .map_err(L1WatcherError::Batch)?;

        tracing::info!(
            protocol_version = ?upgrade_info.protocol_version(),
            target_timestamp = request.timestamp,
            "detected upgrade transaction to be sent"
        );

        // Wait until the timestamp before sending the upgrade tx, so that it's immediately executable.
        // TODO: this will block the watcher, so if e.g. a timestamp is set far in the future, and then an event
        // to override it is emitted, we will not be able to process it.
        self.wait_until_timestamp(request.timestamp).await;

        tracing::info!(
            protocol_version = ?upgrade_info.protocol_version(),
            "sending upgrade transaction to the mempool"
        );

        self.current_protocol_version = upgrade_info.protocol_version().clone();
        self.upgrade_subpool.insert(upgrade_info).await;

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

    util::find_l1_block_by_predicate(Arc::new(zk_chain), 0, move |zk, block| async move {
        let res = zk.get_raw_protocol_version(block.into()).await?;
        Ok(res >= protocol_version)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zkos_hash_matches_blake2s256() {
        let bytecode = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = zkos_hash_from_bytecode(&bytecode);
        let expected = B256::from_slice(Blake2s256::digest(&bytecode).as_slice());
        assert_eq!(result, expected);
    }

    #[test]
    fn zkos_hash_empty_bytecode() {
        let result = zkos_hash_from_bytecode(&[]);
        let expected = B256::from_slice(Blake2s256::digest([]).as_slice());
        assert_eq!(result, expected);
    }

    #[test]
    fn zkos_hash_is_deterministic() {
        let bytecode = b"some evm bytecode here";
        let hash1 = zkos_hash_from_bytecode(bytecode);
        let hash2 = zkos_hash_from_bytecode(bytecode);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn zkos_hash_differs_for_different_bytecodes() {
        let a = zkos_hash_from_bytecode(b"bytecode_a");
        let b = zkos_hash_from_bytecode(b"bytecode_b");
        assert_ne!(a, b);
    }
}
