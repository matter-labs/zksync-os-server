#![feature(allocator_api)]

use alloy::consensus::EMPTY_OMMER_ROOT_HASH;
use alloy::eips::eip1559::INITIAL_BASE_FEE;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use blake2::{Blake2s256, Digest};
use ruint::aliases::B160;
use serde::{Deserialize, Serialize};
use std::alloc::Global;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::OnceCell;
use zk_ee::execution_environment_type::ExecutionEnvironmentType;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, VersioningData, bytecode_padding_len,
};
use zk_os_evm_interpreter::{ARTIFACTS_CACHING_CODE_VERSION_BYTE, BytecodePreprocessingData};
use zk_os_forward_system::run::output::BlockHeader;
use zksync_os_contract_interface::IL1GenesisUpgrade::GenesisUpgrade;
use zksync_os_contract_interface::ZkChain;
use zksync_os_types::L1UpgradeEnvelope;

#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisInput {
    /// Initial contracts to deploy in genesis.
    /// Storage entries that set the contracts as deployed and preimages will be derived from this field.
    pub initial_contracts: Vec<(Address, alloy::primitives::Bytes)>,
    /// Additional (not related to contract deployments) storage entries to add in genesis state.
    pub additional_storage: Vec<(Bytes32, Bytes32)>,
}

/// Struct that represents the genesis state of the system.
/// Lazy-initialized to avoid unnecessary computation at startup.
#[derive(Clone)]
pub struct Genesis {
    input_path: PathBuf,
    l1_provider: DynProvider<Ethereum>,
    zk_chain_address: Address,
    state: OnceLock<GenesisState>,
    // (upgrade tx, force deploy bytecode hashes and preimages)
    genesis_upgrade_tx: OnceCell<(L1UpgradeEnvelope, Vec<(Bytes32, Vec<u8>)>)>,
}

impl Debug for Genesis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Genesis")
            .field("input_path", &self.input_path)
            .field("l1_provider", &self.l1_provider)
            .field("zk_chain_address", &self.zk_chain_address)
            .field("state", &self.state.get())
            .field("inner", &self.genesis_upgrade_tx.get())
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

    pub async fn genesis_upgrade_tx(&self) -> (L1UpgradeEnvelope, Vec<(Bytes32, Vec<u8>)>) {
        self.genesis_upgrade_tx
            .get_or_try_init(|| load_genesis_upgrade_tx(&self.l1_provider, self.zk_chain_address))
            .await
            .expect("Failed to load genesis upgrade transaction")
            .clone()
    }
}

#[derive(Debug, Clone)]
pub struct GenesisState {
    pub storage_logs: Vec<(Bytes32, Bytes32)>,
    pub preimages: Vec<(Bytes32, Vec<u8>)>,
    pub header: BlockHeader,
}

fn build_genesis(genesis_input_path: PathBuf) -> GenesisState {
    let genesis_input: GenesisInput = serde_json::from_reader(
        std::fs::File::open(genesis_input_path).expect("Failed to open genesis input file"),
    )
    .expect("Failed to parse genesis input file");

    let mut storage_logs: BTreeMap<Bytes32, Bytes32> = BTreeMap::new();
    let mut preimages = vec![];

    for (address, deployed_code) in genesis_input.initial_contracts {
        let mut versioning_data = VersioningData::empty_non_deployed();
        versioning_data.set_as_deployed();
        versioning_data.set_code_version(ARTIFACTS_CACHING_CODE_VERSION_BYTE);
        versioning_data.set_ee_version(ExecutionEnvironmentType::EVM as u8);

        let observable_bytecode_hash = {
            let digest = keccak256(&deployed_code);
            Bytes32::from_array(digest.0)
        };
        let observable_bytecode_len = deployed_code.len() as u32;

        let artifacts = BytecodePreprocessingData::create_artifacts_inner(Global, &deployed_code);
        let artifacts = artifacts.as_slice();
        let artifacts_len = artifacts.len() as u32;

        let (bytecode_preimage, bytecode_hash) = {
            let padding_len = bytecode_padding_len(deployed_code.len());
            let padding = [0u8; core::mem::size_of::<u64>() - 1];
            let mut bytecode_preimage =
                Vec::with_capacity(deployed_code.len() + padding_len + artifacts_len as usize);
            bytecode_preimage.extend_from_slice(&deployed_code);
            bytecode_preimage.extend_from_slice(&padding[..padding_len]);
            bytecode_preimage.extend_from_slice(artifacts);

            let digest = Blake2s256::digest(&bytecode_preimage);
            let mut digest_array = [0u8; 32];
            digest_array.copy_from_slice(digest.as_slice());
            (bytecode_preimage, Bytes32::from_array(digest_array))
        };

        let account_properties = AccountProperties {
            versioning_data,
            // When contracts are deployed, they have a nonce of 1
            nonce: 1,
            balance: U256::ZERO,
            bytecode_hash,
            unpadded_code_len: observable_bytecode_len,
            artifacts_len,
            observable_bytecode_hash,
            observable_bytecode_len,
        };

        let flat_storage_key = {
            let mut bytes = [0u8; 64];
            bytes[12..32].copy_from_slice(&ACCOUNT_PROPERTIES_STORAGE_ADDRESS.to_be_bytes::<20>());
            bytes[44..64].copy_from_slice(address.as_slice());

            Bytes32::from_array(B256::from_slice(Blake2s256::digest(bytes).as_slice()).0)
        };
        let account_properties_hash = account_properties.compute_hash();
        storage_logs.insert(flat_storage_key, account_properties_hash);

        preimages.push((bytecode_hash, bytecode_preimage));
        preimages.push((
            account_properties_hash,
            account_properties.encoding().to_vec(),
        ));
    }

    for (key, value) in genesis_input.additional_storage {
        let duplicate = storage_logs.insert(key, value).is_some();
        if duplicate {
            panic!("Genesis input contains duplicate storage key: {key:?}");
        }
    }

    let header = BlockHeader {
        parent_hash: Bytes32::ZERO,
        ommers_hash: Bytes32::from_array(EMPTY_OMMER_ROOT_HASH.0),
        beneficiary: B160::ZERO,
        // for now state root is zero
        state_root: Bytes32::ZERO,
        transactions_root: Bytes32::ZERO,
        receipts_root: Bytes32::ZERO,
        logs_bloom: [0; 256],
        difficulty: U256::ZERO,
        number: 0,
        gas_limit: 5_000,
        gas_used: 0,
        timestamp: 0,
        extra_data: Default::default(),
        mix_hash: Bytes32::ZERO,
        nonce: [0u8; 8],
        base_fee_per_gas: INITIAL_BASE_FEE,
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
) -> anyhow::Result<(L1UpgradeEnvelope, Vec<(Bytes32, Vec<u8>)>)> {
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
                    "Binary search failed with {}. Cannot default starting block to zero for a long chain. Current L1 block number: {}. Limit: {}.",
                    err,
                    current_l1_block,
                    MAX_L1_BLOCKS_LOOKBEHIND,
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
        "Expected exactly one genesis upgrade tx log, found these {:?}",
        logs
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
            (Bytes32::from_array(digest_array), preimage)
        })
        .collect();

    Ok((upgrade_tx, preimages))
}
