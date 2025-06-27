use crate::commitment::StoredBatchInfo;
use alloy::primitives::keccak256;
use zksync_os_contract_interface::Bridgehub;

pub async fn load_genesis_stored_batch(bridgehub: &Bridgehub) -> anyhow::Result<StoredBatchInfo> {
    let genesis_stored_batch = StoredBatchInfo {
        batch_number: 0,
        // TODO: Make dynamic; can this be resolved without genesis configuration?
        // Result of `blake2(0x90a83ead2ba2194fbbb0f7cd2a017e36cfb4891513546d943a7282c2844d4b6b,0x2)`
        state_commitment: "0x6e6e7044cb237fa30937e7e2ee56db7bb3b5d3bd0f46ba7c3a46a6ac4cf2f330"
            .parse()
            .unwrap(),
        number_of_layer1_txs: 0,
        priority_operations_hash: keccak256(&[]).into(),
        // `DEFAULT_L2_LOGS_TREE_ROOT_HASH` is explicitly set to zero in L1 contracts.
        // See `era-contracts/l1-contracts/contracts/common/Config.sol`.
        l2_to_l1_logs_root_hash: Default::default(),
        // TODO: Calculate from empty header
        commitment: "0x753b52ab98b0062963a4b2ea1c061c4ab522f53f50b8fefe0a52760cbcc9e183"
            .parse()
            .unwrap(),
    };
    let genesis_stored_hash = bridgehub.stored_batch_hash(0).await?;
    assert_eq!(genesis_stored_batch.hash(), genesis_stored_hash);

    Ok(genesis_stored_batch)
}
