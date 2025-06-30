use alloy::consensus::transaction::Recovered;
use alloy::consensus::{EthereumTxEnvelope, TxEip4844Variant};
use std::str::FromStr;
use zksync_types::abi::{L2CanonicalTransaction, NewPriorityRequest};
use zksync_types::bytecode::BytecodeHash;
use zksync_types::ethabi::{encode, Token};
use zksync_types::l1::L1Tx;
use zksync_types::{Address, H256, PRIORITY_OPERATION_L2_TX_TYPE, U256};

// TODO: document
pub type L1Transaction = L1Tx;

// TODO: document
pub type L2Transaction = Recovered<L2Envelope>;

// TODO: document
pub type L2Envelope = EthereumTxEnvelope<TxEip4844Variant>;

pub fn tx_abi_encode(tx: L1Transaction) -> Vec<u8> {
    let tx_data: TransactionData = tx.into();
    tx_data.abi_encode()
}

#[derive(Debug, Default, Clone)]
pub(crate) struct TransactionData {
    pub(crate) tx_type: u8,
    pub(crate) from: Address,
    pub(crate) to: Option<Address>,
    pub(crate) gas_limit: U256,
    pub(crate) pubdata_price_limit: U256,
    pub(crate) max_fee_per_gas: U256,
    pub(crate) max_priority_fee_per_gas: U256,
    pub(crate) paymaster: Address,
    pub(crate) nonce: U256,
    pub(crate) value: U256,
    // The reserved fields that are unique for different types of transactions.
    // E.g. nonce is currently used in all transaction, but it should not be mandatory
    // in the long run.
    pub(crate) reserved: [U256; 4],
    pub(crate) data: Vec<u8>,
    pub(crate) signature: Vec<u8>,
    // The factory deps provided with the transaction.
    // Note that *only hashes* of these bytecodes are signed by the user
    // and they are used in the ABI encoding of the struct.
    // TODO: include this into the tx signature as part of SMA-1010
    pub(crate) factory_deps: Vec<H256>,
    pub(crate) paymaster_input: Vec<u8>,
    pub(crate) reserved_dynamic: Vec<u8>,
    // pub(crate) raw_bytes: Option<Vec<u8>>,
}

impl TransactionData {
    pub fn abi_encode(self) -> Vec<u8> {
        let mut res = encode(&[Token::Tuple(vec![
            Token::Uint(U256::from_big_endian(&self.tx_type.to_be_bytes())),
            Token::Address(self.from),
            Token::Address(self.to.unwrap_or_default()),
            Token::Uint(self.gas_limit),
            Token::Uint(self.pubdata_price_limit),
            Token::Uint(self.max_fee_per_gas),
            Token::Uint(self.max_priority_fee_per_gas),
            Token::Address(self.paymaster),
            Token::Uint(self.nonce),
            Token::Uint(self.value),
            Token::FixedArray(self.reserved.iter().copied().map(Token::Uint).collect()),
            Token::Bytes(self.data),
            Token::Bytes(self.signature),
            Token::Array(
                self.factory_deps
                    .into_iter()
                    .map(|dep| Token::FixedBytes(dep.as_bytes().to_vec()))
                    .collect(),
            ),
            Token::Bytes(self.paymaster_input),
            Token::Bytes(self.reserved_dynamic),
        ])]);

        res.drain(0..32);
        res
    }
}

impl From<L1Transaction> for TransactionData {
    fn from(execute_tx: L1Transaction) -> Self {
        let common_data = execute_tx.common_data;
        // TODO: cleanup - double check gas fields, and sender, use constant for tx type
        TransactionData {
            tx_type: 255,
            from: common_data.sender,
            to: execute_tx.execute.contract_address,
            gas_limit: common_data.gas_limit,
            pubdata_price_limit: common_data.gas_per_pubdata_limit,
            max_fee_per_gas: common_data.max_fee_per_gas,
            max_priority_fee_per_gas: U256::zero(),
            paymaster: Address::zero(),
            nonce: U256::from(common_data.serial_id.0),
            value: execute_tx.execute.value,
            reserved: [
                common_data.to_mint,
                U256::from_big_endian(common_data.refund_recipient.as_bytes()),
                U256::zero(),
                U256::zero(),
            ],
            data: execute_tx.execute.calldata,
            signature: vec![],
            factory_deps: execute_tx
                .execute
                .factory_deps
                .iter()
                .map(|b| BytecodeHash::for_bytecode(b).value())
                .collect(),
            paymaster_input: vec![],
            reserved_dynamic: vec![],
            // raw_bytes: execute_tx.raw_bytes.map(|a| a.0),
        }
    }
}

// to be replaced with proper L1 deposit
pub fn forced_deposit_transaction() -> L1Transaction {
    let transaction = L2CanonicalTransaction {
        tx_type: U256::from(PRIORITY_OPERATION_L2_TX_TYPE),
        from: U256::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
        to: U256::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
        gas_limit: U256::from("10000000000"),
        gas_per_pubdata_byte_limit: U256::from(1000),
        max_fee_per_gas: U256::from(1),
        max_priority_fee_per_gas: U256::from(0),
        paymaster: U256::zero(),
        nonce: U256::from(1),
        value: U256::from(100),
        reserved: [
            // `toMint`
            U256::from("100000000000000000000000000000"),
            // `refundRecipient`
            U256::from("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049"),
            U256::from(0),
            U256::from(0),
        ],
        data: vec![],
        signature: vec![],
        factory_deps: vec![],
        paymaster_input: vec![],
        reserved_dynamic: vec![],
    };
    let new_priority_request = NewPriorityRequest {
        tx_id: transaction.nonce,
        tx_hash: transaction.hash().0,
        expiration_timestamp: u64::MAX,
        transaction: Box::new(transaction),
        factory_deps: vec![],
    };
    L1Transaction::try_from(new_priority_request).expect("forced deposit transaction is malformed")
}
