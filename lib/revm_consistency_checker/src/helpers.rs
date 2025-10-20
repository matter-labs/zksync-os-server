use alloy::consensus::Transaction;
use alloy::eips::Typed2718;
use alloy::primitives::Bytes;
use reth_revm::context::TxEnv;
use reth_revm::primitives::TxKind;
use reth_revm::state::Bytecode;
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_os_types::ZkTransaction;
use zksync_revm::ZKsyncTx;
use zksync_revm::transaction::abstraction::ZKsyncTxBuilder;

/// Get unpadded code from full bytecode with artifacts.
pub fn get_unpadded_code(full_bytecode: &[u8], account: &AccountProperties) -> Bytecode {
    Bytecode::new_legacy(Bytes::copy_from_slice(
        &full_bytecode[0..account.unpadded_code_len as usize],
    ))
}

/// Convert a ZkTransaction into a revm TxEnv for REVM re-execution
pub fn zk_tx_into_revm_tx(
    tx: &ZkTransaction,
    gas_used: u64,
    execution_status: bool,
) -> ZKsyncTx<TxEnv> {
    let caller = tx.signer();

    // Extract transaction details based on envelope type
    let envelope = tx.envelope();

    let (
        gas_price,
        gas_priority_fee,
        value,
        data,
        chain_id,
        access_list,
        to_mint,
        refund_recipient,
    ) = match envelope {
        zksync_os_types::ZkEnvelope::L2(l2_tx) => {
            // L2 transactions are standard Ethereum transactions
            let gas_price = l2_tx.max_fee_per_gas();
            let priority_fee = l2_tx.max_priority_fee_per_gas();
            let value = l2_tx.value();
            let data = l2_tx.input().clone();
            let chain_id = l2_tx.chain_id();
            let access_list = l2_tx.access_list().cloned().unwrap_or_default();

            (
                gas_price,
                priority_fee,
                value,
                data,
                chain_id,
                access_list,
                Default::default(),
                None,
            )
        }
        zksync_os_types::ZkEnvelope::L1(l1_tx) => {
            // L1 priority transactions - extract from canonical transaction format
            let inner = &l1_tx.inner;
            (
                l1_tx.max_fee_per_gas(),
                l1_tx.max_priority_fee_per_gas(),
                inner.value(),
                inner.input().clone(),
                None,
                Default::default(), // L1 transactions don't have access lists
                inner.to_mint,
                Some(inner.refund_recipient),
            )
        }
        zksync_os_types::ZkEnvelope::Upgrade(upgrade_tx) => {
            // Upgrade transactions - system-level transactions
            let inner = &upgrade_tx.inner;
            (
                0,
                None,
                inner.value(),
                inner.input().clone(),
                None,
                Default::default(),
                upgrade_tx.inner.to_mint,
                Some(inner.refund_recipient),
            )
        }
    };

    // Determine transaction kind (Call or Create)
    let transact_to = match tx.to() {
        Some(to) => TxKind::Call(to),
        None => TxKind::Create,
    };

    // Build TxEnv using the builder pattern
    let mut tx_env_builder = TxEnv::builder()
        .caller(caller)
        .gas_limit(tx.gas_limit())
        .gas_price(gas_price as u128)
        .kind(transact_to)
        .value(value)
        .data(data)
        .nonce(tx.nonce())
        .access_list(access_list)
        .tx_type(Some(tx.tx_type().ty()))
        .blob_hashes(vec![]); // ZkSync transactions don't use blobs yet

    // Add optional fields
    if let Some(chain) = chain_id {
        tx_env_builder = tx_env_builder.chain_id(Some(chain));
    }

    if let Some(priority_fee) = gas_priority_fee {
        tx_env_builder = tx_env_builder.gas_priority_fee(Some(priority_fee as u128));
    }

    let tx = ZKsyncTxBuilder::new()
        .base(tx_env_builder)
        .mint(to_mint)
        .refund_recipient(refund_recipient)
        .gas_used_override(Some(gas_used))
        .force_fail(!execution_status)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build TxEnv: {:?}", e))
        .unwrap();

    tx
}
