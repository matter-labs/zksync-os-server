use crate::tree_manager::TreeManager;
use crate::{REPOSITORY_DB_NAME, TREE_DB_NAME};
use alloy::consensus::EMPTY_OMMER_ROOT_HASH;
use alloy::eips::eip1559::INITIAL_BASE_FEE;
use alloy::primitives::{Address, B256, U256, keccak256};
use blake2::{Blake2s256, Digest};
use ruint::aliases::B160;
use serde::{Deserialize, Serialize};
use std::alloc::Global;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use zk_ee::execution_environment_type::ExecutionEnvironmentType;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, VersioningData, bytecode_padding_len,
};
use zk_os_evm_interpreter::{ARTIFACTS_CACHING_CODE_VERSION_BYTE, BytecodePreprocessingData};
use zk_os_forward_system::run::output::BlockHeader;
use zksync_os_merkle_tree::TreeEntry;
use zksync_os_state::StateHandle;
use zksync_os_storage::db::RepositoryDb;

#[derive(Debug, Serialize, Deserialize)]
pub struct GenesisInput {
    /// Initial contracts to deploy in genesis.
    /// Storage entries that set the contracts as deployed and preimages will be derived from this field.
    pub initial_contracts: Vec<(Address, alloy::primitives::Bytes)>,
    /// Additional (not related to contract deployments) storage entries to add in genesis state.
    pub additional_storage: Vec<(Bytes32, Bytes32)>,
}

pub struct Genesis {
    pub storage_logs: Vec<(Bytes32, Bytes32)>,
    pub preimages: Vec<(Bytes32, Vec<u8>)>,
    pub header: BlockHeader,
}

pub fn is_genesis_required(rocks_db_path: PathBuf) -> bool {
    StateHandle::requires_genesis(rocks_db_path)
}

pub fn perform_genesis(genesis_input_path: PathBuf, rocks_db_path: PathBuf) -> anyhow::Result<()> {
    let genesis = build_genesis(genesis_input_path);

    // Persist genesis block in tree DB.
    let tree_wrapper =
        TreeManager::tree_wrapper(Path::new(&rocks_db_path.join(TREE_DB_NAME)), true);
    let tree_entries = genesis
        .storage_logs
        .iter()
        .map(|(key, value)| TreeEntry {
            key: B256::from_slice(key.as_u8_ref()),
            value: B256::from_slice(value.as_u8_ref()),
        })
        .collect::<Vec<_>>();
    TreeManager::genesis(tree_wrapper, &tree_entries);

    // Persist block in the repository.
    RepositoryDb::process_genesis(&rocks_db_path.join(REPOSITORY_DB_NAME), genesis.header);

    // Persist genesis state, should be done as last step.
    StateHandle::process_genesis(
        rocks_db_path.clone(),
        genesis.storage_logs,
        genesis.preimages,
    );

    Ok(())
}

fn build_genesis(genesis_input_path: PathBuf) -> Genesis {
    let genesis_input: GenesisInput = serde_json::from_reader(
        std::fs::File::open(genesis_input_path).expect("Failed to open genesis input file"),
    )
    .expect("Failed to parse genesis input file");

    let mut storage_logs: HashMap<Bytes32, Bytes32> = HashMap::new();
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

    Genesis {
        storage_logs: storage_logs.into_iter().collect(),
        preimages,
        header,
    }
}
