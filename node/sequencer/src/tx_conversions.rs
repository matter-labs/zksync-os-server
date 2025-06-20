// we use zksync-era transaction types for now, so we need to convert back and forth.
// we should have a lightweight wrapper for zksync-os (or use some common crate) to avoid this conversion

use crate::conversions::b160_to_address;
use crate::storage::in_memory_tx_receipts::TransactionApiData;
use crate::CHAIN_ID;
use zk_os_forward_system::run::{BatchOutput, ExecutionResult};
use zksync_types::{api, H256, U256, U64};

pub fn transaction_to_api_data(
    block_output: &BatchOutput,
    index: usize,
    tx: &zksync_types::Transaction,
) -> TransactionApiData {
    // let mut ts = std::time::Instant::now();

    let api_tx = api::Transaction {
        hash: tx.hash(),
        nonce: U256::from(tx.nonce().map(|n| n.0).unwrap_or(0)),
        // block_hash: Some(block_output.header.hash().into()),
        block_hash: Some(H256::default()),
        block_number: Some(block_output.header.number.into()),
        transaction_index: Some(index.into()),
        from: Some(tx.initiator_account()),
        to: tx.execute.contract_address,
        gas_price: Some(U256::from(1)),
        gas: U256::from(100),
        chain_id: U256::from(CHAIN_ID),
        value: tx.execute.value,
        transaction_type: Some((tx.tx_format() as u64).into()),
        input: "0x0000".into(),
        r: Some(U256::zero()),
        v: Some(0.into()),
        s: Some(U256::zero()),
        max_fee_per_gas: Some(U256::from(1)),
        y_parity: None,
        max_priority_fee_per_gas: Some(U256::from(1)),
        //todo: other fields
        ..Default::default()
    };

    // tracing::info!("Block {} - saving - api::Transaction in {:?},", block_output.header.number, ts.elapsed());
    // ts = std::time::Instant::now();
    let tx_output = block_output
        .tx_results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .nth(index)
        .expect("mismatch in number of transactions and results");

    let api_receipt = api::TransactionReceipt {
        transaction_hash: tx.hash(),
        transaction_index: index.into(),
        // block_hash: block_output.header.hash().into(),
        block_hash: H256::default(),
        block_number: block_output.header.number.into(),
        l1_batch_tx_index: None,
        l1_batch_number: None,
        from: tx.initiator_account(),
        to: tx.execute.contract_address,
        // todo
        cumulative_gas_used: 7777.into(),
        gas_used: Some(U256::from(tx_output.gas_used)),
        contract_address: tx_output.contract_address.map(b160_to_address),
        logs: vec![],
        l2_to_l1_logs: vec![],
        status: if matches!(tx_output.execution_result, ExecutionResult::Success(_)) {
            U64::from(1)
        } else {
            U64::from(0)
        },
        logs_bloom: Default::default(),
        transaction_type: Some((tx.tx_format() as u32).into()),
        effective_gas_price: Some(block_output.header.base_fee_per_gas.into()),
    };

    // tracing::info!("Block {} - saving - api::TransactionReceipt in {:?},", block_output.header.number, ts.elapsed());
    // ts = std::time::Instant::now();

    TransactionApiData {
        transaction: api_tx,
        receipt: api_receipt,
    }
}
