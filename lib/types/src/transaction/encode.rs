use crate::transaction::L1TxType;
use crate::transaction::l1::L1Envelope;
use crate::transaction::l2::L2Transaction;
use crate::{ZkEnvelope, ZkTransaction};
use alloy::consensus::Transaction;
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolValue;
use zksync_os_interface::traits::EncodedTx;

/// A transaction that can be encoded in ZKsync OS generic transaction format.
///
/// Blanket implementation for `T where TransactionData: From<T>` is available.
pub trait ZksyncOsEncode {
    /// Encode transaction in ZKsync OS generic transaction format. See
    /// `basic_bootloader::bootloader::transaction::ZkSyncTransaction` for the exact spec.
    fn encode(self) -> EncodedTx;
}

impl<T: L1TxType> ZksyncOsEncode for L1Envelope<T> {
    fn encode(self) -> EncodedTx {
        EncodedTx::Abi(TransactionData::from(self).abi_encode())
    }
}

impl ZksyncOsEncode for L2Transaction {
    fn encode(self) -> EncodedTx {
        let (envelope, signer) = self.into_parts();
        EncodedTx::Rlp(envelope.encoded_2718(), signer)
    }
}

impl ZksyncOsEncode for ZkTransaction {
    fn encode(self) -> EncodedTx {
        let (envelope, signer) = self.into_parts();
        match envelope {
            ZkEnvelope::L1(l1_envelope) => l1_envelope.encode(),
            ZkEnvelope::Upgrade(upgrade_envelope) => upgrade_envelope.encode(),
            ZkEnvelope::L2(l2_envelope) => {
                L2Transaction::new_unchecked(l2_envelope, signer).encode()
            }
        }
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

impl<T: L1TxType> From<L1Envelope<T>> for TransactionData {
    fn from(l1_tx: L1Envelope<T>) -> Self {
        let l1_tx = l1_tx.inner;
        TransactionData {
            tx_type: U256::from(T::TX_TYPE),
            from: l1_tx.initiator,
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

impl From<L2Transaction> for TransactionData {
    fn from(l2_tx: L2Transaction) -> Self {
        let (l2_tx, from) = l2_tx.into_parts();
        let nonce = U256::from_be_slice(&l2_tx.nonce().to_be_bytes());

        let should_check_chain_id = if l2_tx.is_legacy() && l2_tx.chain_id().is_some() {
            U256::ONE
        } else {
            U256::ZERO
        };

        let is_deployment_transaction = if l2_tx.is_create() {
            U256::ONE
        } else {
            U256::ZERO
        };

        let encoded_access_list = l2_tx
            .access_list()
            .map(|access_list| {
                let access_list = access_list
                    .clone()
                    .0
                    .into_iter()
                    .map(|item| (item.address, item.storage_keys))
                    .collect::<Vec<_>>();
                // todo(EIP-7702): encode authorization list in second slot
                vec![access_list, vec![]].abi_encode()
            })
            .unwrap_or_default();

        TransactionData {
            tx_type: U256::from(l2_tx.tx_type() as u8),
            from,
            to: l2_tx.to().unwrap_or_default(),
            gas_limit: U256::from(l2_tx.gas_limit()),
            pubdata_price_limit: U256::from(0),
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
            reserved_dynamic: encoded_access_list,
        }
    }
}

impl From<ZkTransaction> for TransactionData {
    fn from(value: ZkTransaction) -> Self {
        let (envelope, signer) = value.into_parts();
        match envelope {
            ZkEnvelope::L1(l1_envelope) => l1_envelope.into(),
            ZkEnvelope::Upgrade(upgrade_envelope) => upgrade_envelope.into(),
            ZkEnvelope::L2(l2_envelope) => L2Transaction::new_unchecked(l2_envelope, signer).into(),
        }
    }
}
