use alloy::primitives::{Address, B64, B256, Bloom, U256};

pub fn alloy_header(
    header: &zk_os_forward_system::run::output::BlockHeader,
) -> alloy::consensus::Header {
    alloy::consensus::Header {
        parent_hash: B256::new(header.parent_hash.as_u8_array()),
        ommers_hash: B256::new(header.ommers_hash.as_u8_array()),
        beneficiary: Address::new(header.beneficiary.to_be_bytes()),
        state_root: B256::new(header.state_root.as_u8_array()),
        transactions_root: B256::new(header.transactions_root.as_u8_array()),
        receipts_root: B256::new(header.receipts_root.as_u8_array()),
        logs_bloom: Bloom::new(header.logs_bloom),
        difficulty: U256::from_be_bytes(header.difficulty.to_be_bytes::<32>()),
        number: header.number,
        gas_limit: header.gas_limit,
        gas_used: header.gas_used,
        timestamp: header.timestamp,
        extra_data: header.extra_data.to_vec().into(),
        mix_hash: B256::new(header.mix_hash.as_u8_array()),
        nonce: B64::new(header.nonce),
        base_fee_per_gas: Some(header.base_fee_per_gas),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
    }
}
