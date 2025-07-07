use alloy::consensus::transaction::{Recovered, RlpEcdsaEncodableTx};
use alloy::consensus::{EthereumTxEnvelope, Transaction, TxEip4844Variant};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolValue;
use zksync_types::bytecode::BytecodeHash;
use zksync_types::l1::L1Tx;

// TODO: document
pub type L1Transaction = L1Tx;

// TODO: document
pub type L2Transaction<Eip4844 = TxEip4844Variant<BlobTransactionSidecarVariant>> =
    Recovered<L2Envelope<Eip4844>>;

// TODO: document
pub type L2Envelope<Eip4844 = TxEip4844Variant<BlobTransactionSidecarVariant>> =
    EthereumTxEnvelope<Eip4844>;

pub trait EncodableZksyncOs {
    /// Encode transaction in ZKsync OS generic transaction format. See
    /// `basic_bootloader::bootloader::transaction::ZkSyncTransaction` for the exact spec.
    fn encode_zksync_os(self) -> Vec<u8>;
}

impl<T> EncodableZksyncOs for T
where
    TransactionData: From<T>,
{
    fn encode_zksync_os(self) -> Vec<u8> {
        TransactionData::from(self).abi_encode()
    }
}

#[derive(Debug, Default, Clone)]
struct TransactionData {
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
    // The reserved fields that are unique for different types of transactions.
    reserved: [U256; 4],
    data: Vec<u8>,
    signature: Vec<u8>,
    // The factory deps provided with the transaction.
    // Note that *only hashes* of these bytecodes are signed by the user
    // and they are used in the ABI encoding of the struct.
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

impl From<L1Transaction> for TransactionData {
    fn from(l1_tx: L1Transaction) -> Self {
        let common_data = l1_tx.common_data;
        // TODO: cleanup - double check gas fields, and sender, use constant for tx type
        TransactionData {
            tx_type: U256::from(255),
            from: Address::from(common_data.sender.0),
            to: l1_tx
                .execute
                .contract_address
                .map(|address| Address::from(address.0))
                .unwrap_or_default(),
            gas_limit: U256::from_limbs(common_data.gas_limit.0),
            pubdata_price_limit: U256::from_limbs(common_data.gas_per_pubdata_limit.0),
            max_fee_per_gas: U256::from_limbs(common_data.max_fee_per_gas.0),
            max_priority_fee_per_gas: U256::ZERO,
            paymaster: Address::ZERO,
            nonce: U256::from(common_data.serial_id.0),
            value: U256::from_limbs(l1_tx.execute.value.0),
            reserved: [
                U256::from_limbs(common_data.to_mint.0),
                U256::from_be_slice(common_data.refund_recipient.as_bytes()),
                U256::ZERO,
                U256::ZERO,
            ],
            data: l1_tx.execute.calldata,
            signature: vec![],
            factory_deps: l1_tx
                .execute
                .factory_deps
                .iter()
                .map(|b| B256::from(BytecodeHash::for_bytecode(b).value().0))
                .collect(),
            paymaster_input: vec![],
            reserved_dynamic: vec![],
        }
    }
}

impl<Eip4844: Transaction + RlpEcdsaEncodableTx> From<L2Envelope<Eip4844>> for TransactionData {
    fn from(l2_tx: L2Envelope<Eip4844>) -> Self {
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
        let (l2_envelope, from) = l2_tx.into_parts();
        let mut tx_data = TransactionData::from(l2_envelope);
        tx_data.from = from;
        tx_data
    }
}
