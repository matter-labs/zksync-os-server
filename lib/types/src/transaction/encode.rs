use crate::transaction::l1::{L1Envelope, REAL_L1_PRIORITY_TX_TYPE_ID};
use crate::transaction::l2::{L2Envelope, L2Transaction};
use crate::{ZkEnvelope, ZkTransaction};
use alloy::consensus::Transaction;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolValue;

/// A transaction that can be encoded in ZKsync OS generic transaction format.
///
/// Blanket implementation for `T where TransactionData: From<T>` is available.
pub trait ZksyncOsEncode {
    /// Encode transaction in ZKsync OS generic transaction format. See
    /// `basic_bootloader::bootloader::transaction::ZkSyncTransaction` for the exact spec.
    fn encode(self) -> Vec<u8>;
}

impl<T> ZksyncOsEncode for T
where
    TransactionData: From<T>,
{
    fn encode(self) -> Vec<u8> {
        TransactionData::from(self).abi_encode()
    }
}

/// ZKsync OS generic transaction format. See `basic_bootloader::bootloader::transaction::ZkSyncTransaction`
/// for the exact spec. To be changed in the future.
#[derive(Debug, Default, Clone)]
pub struct TransactionData {
    tx_type: U256,
    from: Address,
    to: Address,
    gas_limit: U256,
    pubdata_price_limit: U256,
    max_fee_per_gas: U256,
    max_priority_fee_per_gas: U256,
    paymaster: Address,
    nonce: U256,
    value: U256,
    reserved: [U256; 4],
    data: Vec<u8>,
    signature: Vec<u8>,
    factory_deps: Vec<B256>,
    paymaster_input: Vec<u8>,
    reserved_dynamic: Vec<u8>,
}

impl TransactionData {
    pub fn abi_encode(self) -> Vec<u8> {
        (
            self.tx_type,
            self.from,
            self.to,
            self.gas_limit,
            self.pubdata_price_limit,
            self.max_fee_per_gas,
            self.max_priority_fee_per_gas,
            self.paymaster,
            self.nonce,
            self.value,
            self.reserved,
            self.data,
            self.signature,
            self.factory_deps,
            self.paymaster_input,
            self.reserved_dynamic,
        )
            .abi_encode_sequence()
    }
}

impl From<L1Envelope> for TransactionData {
    fn from(l1_tx: L1Envelope) -> Self {
        let (l1_tx, _, _) = l1_tx.inner.into_parts();
        TransactionData {
            tx_type: U256::from(REAL_L1_PRIORITY_TX_TYPE_ID),
            from: l1_tx.from,
            to: l1_tx.to,
            gas_limit: U256::from(l1_tx.gas_limit),
            pubdata_price_limit: U256::from(l1_tx.gas_per_pubdata_byte_limit),
            max_fee_per_gas: U256::from(l1_tx.max_fee_per_gas),
            max_priority_fee_per_gas: U256::from(l1_tx.max_priority_fee_per_gas),
            paymaster: Address::ZERO,
            nonce: U256::from(l1_tx.nonce),
            value: U256::from(l1_tx.value),
            reserved: [
                U256::from(l1_tx.to_mint),
                U256::from_be_slice(l1_tx.refund_recipient.as_slice()),
                U256::ZERO,
                U256::ZERO,
            ],
            data: l1_tx.input.to_vec(),
            signature: vec![],
            factory_deps: l1_tx.factory_deps,
            paymaster_input: vec![],
            reserved_dynamic: vec![],
        }
    }
}

impl From<L2Envelope> for TransactionData {
    fn from(l2_tx: L2Envelope) -> Self {
        let nonce = U256::from_be_slice(&l2_tx.nonce().to_be_bytes());

        let should_check_chain_id = if l2_tx.is_legacy() && l2_tx.chain_id().is_some() {
            U256::ONE
        } else {
            U256::ZERO
        };

        // Ethereum transactions do not sign gas per pubdata limit, and so for them we need to use
        // some default value. We use the maximum possible value that is allowed by the bootloader
        // (i.e. we can not use u64::MAX, because the bootloader requires gas per pubdata for such
        // transactions to be higher than `MAX_GAS_PER_PUBDATA_BYTE`).
        let gas_per_pubdata_limit = 50_000;

        let is_deployment_transaction = if l2_tx.is_create() {
            U256::ONE
        } else {
            U256::ZERO
        };

        TransactionData {
            tx_type: U256::from(l2_tx.tx_type() as u8),
            from: Address::ZERO,
            to: l2_tx.to().unwrap_or_default(),
            gas_limit: U256::from(l2_tx.gas_limit()),
            pubdata_price_limit: U256::from(gas_per_pubdata_limit),
            max_fee_per_gas: U256::from(l2_tx.max_fee_per_gas()),
            max_priority_fee_per_gas: U256::from(
                l2_tx
                    .max_priority_fee_per_gas()
                    .unwrap_or_else(|| l2_tx.max_fee_per_gas()),
            ),
            paymaster: Address::ZERO,
            nonce,
            value: l2_tx.value(),
            reserved: [
                should_check_chain_id,
                is_deployment_transaction,
                U256::ZERO,
                U256::ZERO,
            ],
            data: l2_tx.input().to_vec(),
            signature: l2_tx.signature().as_bytes().to_vec(),
            factory_deps: vec![],
            paymaster_input: vec![],
            reserved_dynamic: vec![],
        }
    }
}

impl From<L2Transaction> for TransactionData {
    fn from(l2_tx: L2Transaction) -> Self {
        let (l2_tx, from) = l2_tx.into_parts();
        let nonce = U256::from_be_slice(&l2_tx.nonce().to_be_bytes());

        let should_check_chain_id = if l2_tx.is_legacy() && l2_tx.chain_id().is_some() {
            U256::ONE
        } else {
            U256::ZERO
        };

        // Ethereum transactions do not sign gas per pubdata limit, and so for them we need to use
        // some default value. We use the maximum possible value that is allowed by the bootloader
        // (i.e. we can not use u64::MAX, because the bootloader requires gas per pubdata for such
        // transactions to be higher than `MAX_GAS_PER_PUBDATA_BYTE`).
        let gas_per_pubdata_limit = 50_000;

        let is_deployment_transaction = if l2_tx.is_create() {
            U256::ONE
        } else {
            U256::ZERO
        };

        TransactionData {
            tx_type: U256::from(l2_tx.tx_type() as u8),
            from,
            to: l2_tx.to().unwrap_or_default(),
            gas_limit: U256::from(l2_tx.gas_limit()),
            pubdata_price_limit: U256::from(gas_per_pubdata_limit),
            max_fee_per_gas: U256::from(l2_tx.max_fee_per_gas()),
            max_priority_fee_per_gas: U256::from(
                l2_tx
                    .max_priority_fee_per_gas()
                    .unwrap_or_else(|| l2_tx.max_fee_per_gas()),
            ),
            paymaster: Address::ZERO,
            nonce,
            value: l2_tx.value(),
            reserved: [
                should_check_chain_id,
                is_deployment_transaction,
                U256::ZERO,
                U256::ZERO,
            ],
            data: l2_tx.input().to_vec(),
            signature: l2_tx.signature().as_bytes().to_vec(),
            factory_deps: vec![],
            paymaster_input: vec![],
            reserved_dynamic: vec![],
        }
    }
}

impl From<ZkTransaction> for TransactionData {
    fn from(value: ZkTransaction) -> Self {
        let (l2_envelope, signer) = value.into_parts();
        match l2_envelope {
            ZkEnvelope::L1(l1_envelope) => l1_envelope.into(),
            ZkEnvelope::L2(l2_envelope) => L2Transaction::new_unchecked(l2_envelope, signer).into(),
        }
    }
}
