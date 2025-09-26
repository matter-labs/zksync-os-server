use alloy::consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy::eips::eip1559::INITIAL_BASE_FEE;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B64, B256, Bloom, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::OnceCell;
use zk_os_api::helpers::{set_properties_code, set_properties_nonce};
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties,
};
use zksync_os_contract_interface::IL1GenesisUpgrade::GenesisUpgrade;
use zksync_os_contract_interface::ZkChain;
use zksync_os_types::L1UpgradeEnvelope;

#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisInput {
    /// Initial contracts to deploy in genesis.
    /// Storage entries that set the contracts as deployed and preimages will be derived from this field.
    pub initial_contracts: Vec<(Address, alloy::primitives::Bytes)>,
    /// Additional (not related to contract deployments) storage entries to add in genesis state.
    pub additional_storage: Vec<(B256, B256)>,
}

/// Info about genesis upgrade fetched from L1:
/// - genesis upgrade tx
/// - force deploy bytecode hashes and preimages, note that preimages are not padded and do not contain artifacts
#[derive(Debug, Clone)]
pub struct GenesisUpgradeTxInfo {
    pub tx: L1UpgradeEnvelope,
    pub force_deploy_preimages: Vec<(B256, Vec<u8>)>,
}

/// Struct that represents the genesis state of the system.
/// Lazy-initialized to avoid unnecessary computation at startup.
#[derive(Clone)]
pub struct Genesis {
    input_path: PathBuf,
    l1_provider: DynProvider<Ethereum>,
    zk_chain_address: Address,
    state: OnceLock<GenesisState>,
    genesis_upgrade_tx: OnceCell<GenesisUpgradeTxInfo>,
}

impl Debug for Genesis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Genesis")
            .field("input_path", &self.input_path)
            .field("l1_provider", &self.l1_provider)
            .field("zk_chain_address", &self.zk_chain_address)
            .field("state", &self.state.get())
            .field("genesis_upgrade_tx", &self.genesis_upgrade_tx.get())
            .finish()
    }
}

impl Genesis {
    pub fn new(
        input_path: PathBuf,
        l1_provider: DynProvider<Ethereum>,
        zk_chain_address: Address,
    ) -> Self {
        Self {
            input_path,
            l1_provider,
            zk_chain_address,
            state: OnceLock::new(),
            genesis_upgrade_tx: OnceCell::new(),
        }
    }

    pub fn state(&self) -> &GenesisState {
        self.state
            .get_or_init(|| build_genesis(self.input_path.clone()))
    }

    pub async fn genesis_upgrade_tx(&self) -> GenesisUpgradeTxInfo {
        self.genesis_upgrade_tx
            .get_or_try_init(|| load_genesis_upgrade_tx(&self.l1_provider, self.zk_chain_address))
            .await
            .expect("Failed to load genesis upgrade transaction")
            .clone()
    }
}

#[derive(Debug, Clone)]
pub struct GenesisState {
    /// Storage logs for the genesis block.
    pub storage_logs: Vec<(B256, B256)>,
    /// Preimages of the padded bytecodes with artifacts and hashes of account properties
    /// for the contracts deployed in the genesis block.
    /// Note: these preimages don't include `force_deploy_preimages` -
    /// see `genesis_upgrade_tx` method for details
    pub preimages: Vec<(B256, Vec<u8>)>,
    /// The header of the genesis block.
    pub header: Header,
}

fn build_genesis(genesis_input_path: PathBuf) -> GenesisState {
    let genesis_input: GenesisInput = serde_json::from_reader(
        std::fs::File::open(genesis_input_path).expect("Failed to open genesis input file"),
    )
    .expect("Failed to parse genesis input file");

    // BTreeMap is used to ensure that the storage logs are sorted by key, so that the order is deterministic
    // which is important for tree.
    let mut storage_logs: BTreeMap<B256, B256> = BTreeMap::new();
    let mut preimages = vec![];

    for (address, deployed_code) in genesis_input.initial_contracts {
        let mut account_properties = AccountProperties::default();
        // When contracts are deployed, they have a nonce of 1.
        set_properties_nonce(&mut account_properties, 1);
        let bytecode_preimage = set_properties_code(&mut account_properties, &deployed_code);
        let bytecode_hash = account_properties.bytecode_hash;

        let flat_storage_key = {
            let mut bytes = [0u8; 64];
            bytes[12..32].copy_from_slice(&ACCOUNT_PROPERTIES_STORAGE_ADDRESS.to_be_bytes::<20>());
            bytes[44..64].copy_from_slice(address.as_slice());

            B256::from_slice(Blake2s256::digest(bytes).as_slice())
        };
        let account_properties_hash = account_properties.compute_hash();
        storage_logs.insert(
            flat_storage_key,
            account_properties_hash.as_u8_array().into(),
        );

        preimages.push((bytecode_hash.as_u8_array().into(), bytecode_preimage));
        preimages.push((
            account_properties_hash.as_u8_array().into(),
            account_properties.encoding().to_vec(),
        ));
    }

    for (key, value) in genesis_input.additional_storage {
        let duplicate = storage_logs.insert(key, value).is_some();
        if duplicate {
            panic!("Genesis input contains duplicate storage key: {key:?}");
        }
    }

    let header = Header {
        parent_hash: B256::ZERO,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: Address::ZERO,
        // for now state root is zero
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::ZERO,
        difficulty: U256::ZERO,
        number: 0,
        gas_limit: 5_000,
        gas_used: 0,
        timestamp: 0,
        extra_data: Default::default(),
        mix_hash: B256::ZERO,
        nonce: B64::ZERO,
        base_fee_per_gas: Some(INITIAL_BASE_FEE),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
    };

    GenesisState {
        storage_logs: storage_logs.into_iter().collect(),
        preimages,
        header,
    }
}

async fn load_genesis_upgrade_tx(
    provider: &DynProvider<Ethereum>,
    zk_chain_address: Address,
) -> anyhow::Result<GenesisUpgradeTxInfo> {
    const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

    let zk_chain = ZkChain::new(zk_chain_address, provider.clone());
    let current_l1_block = provider.get_block_number().await?;
    // Find the block when the zk chain was deployed or fallback to [0; latest_block] in localhost case.
    let (from_block, to_block) = zksync_os_l1_watcher::util::find_l1_block_by_predicate(
            Arc::new(zk_chain),
            |_zk, _block| async { Ok(true) },
        )
        .await
        .map(|b| (b, b))
        .or_else(|err| {
            // This may error on Anvil with `--load-state` - as it doesn't support requests even for recent blocks.
            // We default to `[0; latest_block]` in this case - `eth_getLogs` are still supported.
            // Assert that we don't fallback on longer chains (e.g. Sepolia)
            if current_l1_block > MAX_L1_BLOCKS_LOOKBEHIND {
                anyhow::bail!(
                    "Binary search failed with {err}. Cannot default starting block to zero for a long chain. Current L1 block number: {current_l1_block}. Limit: {MAX_L1_BLOCKS_LOOKBEHIND}."
                )
            } else {
                Ok((0, current_l1_block))
            }
        })?;
    let event_sig = GenesisUpgrade::SIGNATURE_HASH;
    let filter = Filter::new()
        .from_block(from_block)
        .to_block(to_block)
        .event_signature(event_sig)
        .address(zk_chain_address);
    let logs = provider.get_logs(&filter).await?;
    anyhow::ensure!(
        logs.len() == 1,
        "Expected exactly one genesis upgrade tx log, found these {logs:?}"
    );
    let sol_event = GenesisUpgrade::decode_log(&logs[0].inner)?.data;
    let upgrade_tx = L1UpgradeEnvelope::try_from(sol_event._l2Transaction)?;
    let preimages = sol_event
        ._factoryDeps
        .into_iter()
        .map(|preimage| {
            let preimage = preimage.to_vec();
            let digest = Blake2s256::digest(&preimage);
            let mut digest_array = [0u8; 32];
            digest_array.copy_from_slice(digest.as_slice());
            (B256::new(digest_array), preimage)
        })
        .collect();

    Ok(GenesisUpgradeTxInfo {
        tx: upgrade_tx,
        force_deploy_preimages: preimages,
    })
}
