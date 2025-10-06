use alloy::consensus::Transaction;
use alloy::eips::Typed2718;
use anyhow::Result;
use revm::context::{TxEnv};
use revm::primitives::{TxKind, U256};
use zksync_os_types::ZkTransaction;
use zksync_revm::transaction::abstraction::ZKsyncTxBuilder;
use zksync_revm::ZKsyncTx;

/// Convert a ZkTransaction into a revm TxEnv for EVM execution
pub fn zk_transaction_to_tx_env(tx: &ZkTransaction) -> Result<(ZKsyncTx<TxEnv>, U256, bool)> {
    let caller = tx.signer();

    // Extract transaction details based on envelope type
    let envelope = tx.envelope();
    
    let (gas_price, gas_priority_fee, value, data, chain_id, access_list, to_mint, is_l1) = match envelope {
        zksync_os_types::ZkEnvelope::L2(l2_tx) => {
            // L2 transactions are standard Ethereum transactions
            let gas_price = l2_tx.gas_price().unwrap_or(0);
            let priority_fee = l2_tx.max_priority_fee_per_gas();
            let value = l2_tx.value();
            let data = l2_tx.input().clone();
            let chain_id = l2_tx.chain_id();
            let access_list = l2_tx.access_list().cloned().unwrap_or_default();
            
            (gas_price, priority_fee, value, data, chain_id, access_list, Default::default(), false)
        }
        zksync_os_types::ZkEnvelope::L1(l1_tx) => {
            // L1 priority transactions - extract from canonical transaction format
            use alloy::consensus::Transaction;
            let inner = &l1_tx.inner;
            (
                0u128, // L1 transactions don't have gas price in the same way
                None,
                inner.value(),
                inner.input().clone(),
                inner.chain_id(),
                Default::default(), // L1 transactions don't have access lists
                l1_tx.inner.tx().to_mint,
                true
            )
        }
        zksync_os_types::ZkEnvelope::Upgrade(upgrade_tx) => {
            // Upgrade transactions - system-level transactions
            use alloy::consensus::Transaction;
            let inner = &upgrade_tx.inner;
            (
                0,
                None,
                inner.value(),
                inner.input().clone(),
                inner.chain_id(),
                Default::default(),
                upgrade_tx.inner.tx().to_mint,
                true
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

    // TODO: set refunt recipient
    let op_tx = ZKsyncTxBuilder::new().base(tx_env_builder).mint(to_mint).build().map_err(|e| anyhow::anyhow!("Failed to build TxEnv: {:?}", e)).unwrap();

    Ok((op_tx, to_mint, is_l1))
}
