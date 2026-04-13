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
use blake2::{Blake2s256, Digest};
use zksync_os_contract_interface::IBytecodeSupplier::EVMBytecodePublished;
use zksync_os_contract_interface::IChainAdmin::UpdateUpgradeTimestamp;
use zksync_os_contract_interface::IChainTypeManager::{NewUpgradeCutData, ProposedUpgrade};
use zksync_os_contract_interface::ZkChain;
use zksync_os_mempool::subpools::upgrade::UpgradeSubpool;
use zksync_os_types::{
    L1UpgradeEnvelope, ProtocolSemanticVersion, ProtocolSemanticVersionError, UpgradeInfo,
    UpgradeMetadata,
};

use zksync_os_contract_interface::IChainTypeManager::IChainTypeManagerInstance;

/// Limit the number of L1 blocks to scan when looking for the set timestamp transaction.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;
/// The constant value is higher than for other watchers, since we're looking for rare/specific events
/// and we don't expect a lot of results.
const UPGRADE_DATA_LOOKBEHIND_BLOCKS: u64 = 2_500_000;

/// Watches L1 and the settlement layer (SL) for protocol upgrade data.
///
/// When settling on Gateway, the upgrade data is split across two layers:
/// - **Gateway (SL)**: `NewUpgradeCutData` / `NewUpgradeCutHash` events from `ChainTypeManager`,
///   plus the full upgrade execution including the L2 upgrade transaction.
/// - **L1**: The `BytecodesSupplier` publishes the factory dep bytecodes via
///   `EVMBytecodePublished` events — these live on L1 regardless of settlement layer.
pub struct L1UpgradeTxWatcher {
    admin_contract_l1: Address,

    provider_l1: DynProvider,
    provider_sl: DynProvider,
    /// Address of the bytecode supplier contract on L1 (used to scan EVMBytecodePublished events)
    bytecode_supplier_address: Address,
    /// Address of the CTM contract on L1 (used to resolve the canonical bytecode supplier)
    ctm_l1: Address,
    /// Address of the CTM contract on SL (used to scan NewUpgradeCutData events)
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

        let ctm_l1 = zk_chain_l1.get_chain_type_manager().await?;
        tracing::info!(ctm_l1 = ?ctm_l1, "resolved L1 chain type manager");

        let ctm_sl = zk_chain_sl.get_chain_type_manager().await?;
        tracing::info!(ctm_sl = ?ctm_sl, "resolved SL chain type manager");

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
        // The configured bytecode supplier address is used as fallback for pre-v31 CTMs.
        // On v31+ CTMs, `resolve_active_bytecode_supplier` discovers the address dynamically.
        // Sanity check: make sure the fallback address has code deployed.
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
            ctm_l1,
            ctm_sl,
            current_protocol_version,
            upgrade_subpool,
            max_blocks_to_process: config.max_blocks_to_process,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain_l1.provider().clone(),
            last_l1_block,
            config.max_blocks_to_process,
            config.confirmations,
            zk_chain_l1.provider().get_chain_id().await?,
            config.poll_interval,
            this.into(),
        )
        .await?;

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
            let force_preimages = self.fetch_force_preimages(&tx.inner.factory_deps).await?;

            tracing::info!(
                num_preimages = force_preimages.len(),
                "fetched force deployment preimages from bytecode supplier"
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

    /// Fetches bytecodes published to the `BytecodesSupplier` on L1, filtered by the
    /// requested `hashes` (the `factory_deps` array from the upgrade tx).
    ///
    /// # The hash zoo
    ///
    /// EVM bytecode in this codebase is referenced by *three* distinct hashes; reading
    /// them carelessly is how the "Iterator length exceeds expected preimage length"
    /// panic was originally introduced. They are:
    ///
    /// 1. **`keccak256(raw_bytecode)`** — the "observable" hash. Returned by the EVM
    ///    `EXTCODEHASH` opcode and the value `BytecodesSupplier.publishEVMBytecode`
    ///    indexes its event by (see `era-contracts/.../BytecodesSupplier.sol:64`,
    ///    which calls `ZKSyncOSBytecodeInfo.hashEVMBytecodeCalldata` — that is just
    ///    `keccak256(_bytecode)`). Useful for `EXTCODEHASH` parity but **NOT** the
    ///    key our preimage cache uses.
    ///
    /// 2. **`Blake2s256(raw_bytecode)`** — the "raw blake hash". This is the value
    ///    the era-contracts docstring at `ZKSyncOSBytecodeInfo.sol:30` calls
    ///    `_bytecodeBlakeHash` (NB: docstring says "Blake2b" but the VM uses
    ///    Blake2s). It is the **lookup key** the `set_bytecode_on_address` system
    ///    hook on L2 hands to the preimage cache (`zksync-os/system_hooks/.../
    ///    set_bytecode_on_address.rs:178` → `account_cache.rs:933`), with
    ///    `expected_preimage_len_in_bytes = observable_len`. The corresponding
    ///    preimage body the cache must return is exactly the raw bytes (length =
    ///    observable len). This is also what each entry in `factory_deps` of the
    ///    upgrade tx must equal so that we can fetch the matching preimage here.
    ///
    /// 3. **`Blake2s256(raw + padding + artifacts)`** — the "padded blake hash",
    ///    a.k.a. `account_properties.bytecode_hash`. This is what `set_properties_code`
    ///    in `zksync-os/api/src/helpers.rs:78` computes. The system hook *recomputes*
    ///    this hash in-VM from the raw bytecode (after deriving EVM artifacts via
    ///    `evm_interpreter::BytecodePreprocessingData::create_artifacts`), records a
    ///    fresh padded preimage under it, and writes it into `account.bytecode_hash`.
    ///    All later same-account bytecode lookups in zksync-os go through hash (3),
    ///    but **on the L1 wire it never appears** — neither the supplier nor the
    ///    upgrade tx carries it. Mistaking (3) for (2) was the root cause of the
    ///    panic this function used to trigger.
    ///
    /// # The preimage shape we must store
    ///
    /// The body inserted into the preimage source must be the **raw bytecode**
    /// (length = observable len), keyed by hash (2). Two reasons:
    ///
    /// - **Length check.** `expose_preimage` in `preimage_cache.rs:116` rejects any
    ///   iterator longer than `num_usize_words_for_u8_capacity(expected_len)`. With
    ///   `expected_len = observable_len`, anything bigger (e.g. raw + padding +
    ///   artifacts, length = `full_bytecode_len`) panics with the namesake error.
    /// - **Proof-env hash check.** Same file, line 144: `Blake2s256(buffered) ==
    ///   hash`, where `buffered` is exactly `expected_len` bytes. So the stored
    ///   body must be raw bytes and the lookup hash must be `Blake2s256(raw)` —
    ///   which is hash (2).
    ///
    /// EVM artifacts and the 8-byte word padding are *derived in-VM* by the hook and
    /// stored as a *separate* preimage under hash (3); they never travel on L1.
    /// That's a deliberate design choice — it keeps `BytecodesSupplier` publication
    /// cost proportional to the raw bytecode length, and removes any cross-node
    /// disagreement risk on artifact format.
    ///
    /// # Why we scan instead of filtering
    ///
    /// `EVMBytecodePublished` is indexed by hash (1) (`keccak256`), not hash (2)
    /// (`Blake2s256`). Our `factory_deps` carries hash (2). We can't translate
    /// between them without the bytecode itself, so we have to scan all events in
    /// range and recompute Blake2s on each payload. If the supplier ever gains a
    /// second event indexed by hash (2), this can become a topic-filtered lookup.
    async fn fetch_force_preimages(&self, hashes: &[B256]) -> anyhow::Result<Vec<(B256, Vec<u8>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let active_supplier = self.resolve_active_bytecode_supplier().await?;

        let mut current_block = self.provider_l1.get_block_number().await?;
        let start_block = current_block
            .saturating_sub(UPGRADE_DATA_LOOKBEHIND_BLOCKS)
            .max(1u64);

        // `requested` and the `factory_deps` slice both contain hash (2) values —
        // the Blake2s-of-raw lookup keys the system hook will use on L2.
        let requested: std::collections::HashSet<B256> = hashes.iter().copied().collect();
        let mut by_hash: HashMap<B256, Vec<u8>> = HashMap::new();

        while current_block >= start_block {
            let from_block = current_block
                .saturating_sub(self.max_blocks_to_process - 1)
                .max(start_block);
            // The `EVMBytecodePublished` event is indexed by hash (1) (keccak256
            // of the raw bytes), which is **not** the hash we're filtering by, so
            // we don't add a topic1 filter — we'd have to translate from hash (2)
            // to hash (1) which requires the bytecode itself. Instead we pull all
            // events in the range and discard non-matches inside the loop.
            let filter = Filter::new()
                .from_block(from_block)
                .to_block(current_block)
                .address(active_supplier)
                .event_signature(EVMBytecodePublished::SIGNATURE_HASH);
            let logs = self.provider_l1.get_logs(&filter).await?;

            for log in logs {
                let published = EVMBytecodePublished::decode_log(&log.inner)?.data;
                let raw_bytecode = published.bytecode.to_vec();

                // Compute hash (2) from the event payload so we can match against
                // `requested`. We deliberately do NOT use `set_properties_code`
                // here — that would produce hash (3), which would silently fail to
                // match anything in `factory_deps` (or, worse, match it via a
                // misconfigured upgrade tx and then panic the VM on length).
                let zkos_hash = B256::from_slice(Blake2s256::digest(&raw_bytecode).as_slice());

                // Drop events that publish bytecodes the upgrade tx doesn't ask for.
                if !requested.contains(&zkos_hash) {
                    continue;
                }

                if let Some(existing) = by_hash.get(&zkos_hash) {
                    if existing != &raw_bytecode {
                        // Two different payloads producing the same Blake2s would
                        // be a Blake2s collision (or a bug in the supplier). The
                        // first occurrence wins so we stay deterministic.
                        tracing::warn!(
                            hash = ?zkos_hash,
                            "bytecode supplier emitted duplicate hash with different data; keeping first occurrence"
                        );
                    }
                } else {
                    // Insert the **raw** bytecode (shape A — length = observable
                    // len). The system hook will derive the padded shape B
                    // internally and key it by hash (3) on the account.
                    by_hash.insert(zkos_hash, raw_bytecode);
                }
            }

            // Stop early once all requested hashes are found.
            if by_hash.len() == requested.len() {
                break;
            }
            current_block = from_block.saturating_sub(1);
        }

        // Verify all requested hashes were found.
        let missing: Vec<_> = hashes
            .iter()
            .filter(|h| !by_hash.contains_key(*h))
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "missing {} factory dep preimage(s) from bytecode supplier: {:?}",
            missing.len(),
            missing
        );

        tracing::info!(
            supplier = ?active_supplier,
            num_preimages = by_hash.len(),
            "fetched force deployment preimages from bytecode supplier"
        );

        Ok(by_hash.into_iter().collect())
    }

    /// Queries the CTM on L1 for the canonical `BytecodesSupplier` address.
    async fn resolve_active_bytecode_supplier(&self) -> anyhow::Result<Address> {
        let ctm = IChainTypeManagerInstance::new(self.ctm_l1, self.provider_l1.clone());
        match ctm.L1_BYTECODES_SUPPLIER().call().await {
            Ok(l1_address) if l1_address != Address::ZERO => Ok(l1_address),
            Ok(_) => {
                anyhow::bail!(
                    "L1 ChainTypeManager at {:?} returned zero BytecodesSupplier address",
                    self.ctm_l1
                );
            }
            Err(e) => {
                // Transport errors (503, timeout, etc.) should propagate.
                // Contract-level reverts (function not found) are expected on pre-v31 CTMs.
                if matches!(e, alloy::contract::Error::TransportError(_)) {
                    return Err(e.into());
                }
                tracing::info!(
                    configured_supplier = ?self.bytecode_supplier_address,
                    ctm = ?self.ctm_l1,
                    "CTM does not expose L1_BYTECODES_SUPPLIER(); using configured supplier"
                );
                Ok(self.bytecode_supplier_address)
            }
        }
    }
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
    use blake2::{Blake2s256, Digest as BlakeDigest};
    use zk_os_api::helpers::set_properties_code;
    use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;

    /// Golden-value test using a known externally-verifiable result.
    /// `blake2s256(b"") = 69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9`
    #[test]
    fn blake2s_empty_golden_value() {
        let expected: B256 = "0x69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"
            .parse()
            .unwrap();
        let result = B256::from_slice(Blake2s256::digest([]).as_slice());
        assert_eq!(result, expected);
    }

    #[test]
    fn set_properties_code_produces_expected_hash() {
        // Minimal EVM bytecode: PUSH1 0x42 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        // padded to an odd number of 32-byte words
        let bytecode = vec![0x60, 0x42, 0x5F, 0x52, 0x60, 0x20, 0x5F, 0xF3];

        let mut props = AccountProperties::default();
        let full_preimage = set_properties_code(&mut props, &bytecode);
        let hash = B256::from(props.bytecode_hash.as_u8_array());

        // The full preimage should be longer than the raw bytecode (includes padding + artifacts).
        assert!(full_preimage.len() > bytecode.len());
        // The hash should be blake2s256 of the full preimage.
        let expected_hash = B256::from_slice(Blake2s256::digest(&full_preimage).as_slice());
        assert_eq!(hash, expected_hash);
    }
}
