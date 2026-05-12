use alloy::consensus::Transaction;
use alloy::eips::Typed2718;
use alloy::primitives::{Bytes, U256};
use revm::context::TxEnv;
use revm::primitives::TxKind;
use revm::state::Bytecode;
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_os_revm::transaction::abstraction::ZKsyncTxBuilder;
use zksync_os_revm::{ZKsyncTx, ZkSpecId};
use zksync_os_types::{ExecutionVersion, ZkTransaction};

/// Get unpadded code from full bytecode with artifacts.
pub fn get_unpadded_code(full_bytecode: &[u8], account: &AccountProperties) -> Bytecode {
    Bytecode::new_legacy(Bytes::copy_from_slice(
        &full_bytecode[0..account.unpadded_code_len as usize],
    ))
}

/// Convert a ZkTransaction into a revm TxEnv for REVM re-execution.
///
/// `block_gas_limit` is used for system txs, whose own `gas_limit` is 0;
/// the new revm rejects the tx if `gas_used_override` exceeds `gas_limit`.
pub fn zk_tx_into_revm_tx(
    tx: &ZkTransaction,
    gas_used: u64,
    execution_status: bool,
    block_gas_limit: u64,
    settlement_layer_chain_id: Option<U256>,
) -> anyhow::Result<ZKsyncTx<TxEnv>> {
    let caller = tx.signer();

    let envelope = tx.envelope();

    let mut blob_hashes = vec![];
    let mut max_fee_per_blob_gas = 0;
    let mut authorization_list = vec![];

    let (
        gas_price,
        gas_priority_fee,
        value,
        data,
        chain_id,
        access_list,
        to_mint,
        refund_recipient,
        gas_limit,
    ) = match envelope {
        zksync_os_types::ZkEnvelope::System(system_tx) => (
            0,
            Some(0),
            U256::ZERO,
            system_tx.input().clone(),
            None,
            Default::default(),
            Default::default(),
            None,
            block_gas_limit,
        ),
        zksync_os_types::ZkEnvelope::L2(l2_tx) => {
            let gas_price = l2_tx.max_fee_per_gas();
            let priority_fee = l2_tx.max_priority_fee_per_gas();
            let value = l2_tx.value();
            let data = l2_tx.input().clone();
            let chain_id = l2_tx.chain_id();
            let access_list = l2_tx.access_list().cloned().unwrap_or_default();
            blob_hashes = l2_tx
                .blob_versioned_hashes()
                .map(|hashes| hashes.to_vec())
                .unwrap_or_default();
            max_fee_per_blob_gas = l2_tx.max_fee_per_blob_gas().unwrap_or_default();
            authorization_list = l2_tx
                .authorization_list()
                .map(|list| list.to_vec())
                .unwrap_or_default();

            (
                gas_price,
                priority_fee,
                value,
                data,
                chain_id,
                access_list,
                Default::default(),
                None,
                tx.gas_limit(),
            )
        }
        zksync_os_types::ZkEnvelope::L1(l1_tx) => {
            let inner = &l1_tx.inner;
            (
                l1_tx.max_fee_per_gas(),
                l1_tx.max_priority_fee_per_gas(),
                inner.value(),
                inner.input().clone(),
                None,
                Default::default(),
                inner.to_mint,
                Some(inner.refund_recipient),
                tx.gas_limit(),
            )
        }
        zksync_os_types::ZkEnvelope::Upgrade(upgrade_tx) => {
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
                tx.gas_limit(),
            )
        }
    };

    let transact_to = match tx.to() {
        Some(to) => TxKind::Call(to),
        None => TxKind::Create,
    };

    // The `tx_type == 0x7d` value already triggers the service-tx code path in
    // zksync-os-revm (validation skipped, no nonce/balance checks); no extra
    // builder flag is required.
    let mut tx_env_builder = TxEnv::builder()
        .caller(caller)
        .gas_limit(gas_limit)
        .gas_price(gas_price)
        .kind(transact_to)
        .value(value)
        .data(data)
        .nonce(tx.nonce())
        .access_list(access_list)
        .tx_type(Some(tx.tx_type().ty()))
        .chain_id(chain_id)
        .blob_hashes(blob_hashes)
        .max_fee_per_blob_gas(max_fee_per_blob_gas)
        .authorization_list_signed(authorization_list);

    if let Some(priority_fee) = gas_priority_fee {
        tx_env_builder = tx_env_builder.gas_priority_fee(Some(priority_fee));
    }

    ZKsyncTxBuilder::new()
        .base(tx_env_builder)
        .mint(to_mint)
        .refund_recipient(refund_recipient)
        .settlement_layer_chain_id(settlement_layer_chain_id)
        .gas_used_override(Some(gas_used))
        .force_fail(!execution_status)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build TxEnv: {e:?}"))
}

pub fn zk_spec_version(execution_version: ExecutionVersion) -> Option<ZkSpecId> {
    match execution_version {
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            Some(ZkSpecId::AtlasV1)
        }
        ExecutionVersion::V4 | ExecutionVersion::V5 => Some(ZkSpecId::AtlasV2),
        ExecutionVersion::V6 => Some(ZkSpecId::AtlasV3),
    }
}
