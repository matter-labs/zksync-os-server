mod transaction;

pub use transaction::{EncodableZksyncOs, L1Transaction, L2Envelope, L2Transaction};

use std::str::FromStr;
use zksync_types::abi::{L2CanonicalTransaction, NewPriorityRequest};
use zksync_types::{PRIORITY_OPERATION_L2_TX_TYPE, U256};

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
