mod transaction;

pub use transaction::{
    L1Envelope, L1EnvelopeError, L2Envelope, L2Transaction, TxL1Priority, ZkEnvelope,
    ZkTransaction, ZkTxType, ZksyncOsEncode,
};

use crate::transaction::REAL_L1_PRIORITY_TX_TYPE_ID;
use alloy::primitives::{Bytes, U256};
use std::str::FromStr;
use zksync_os_contract_interface::L2CanonicalTransaction;

// to be replaced with proper L1 deposit
pub fn forced_deposit_transaction() -> L1Envelope {
    let tx = L2CanonicalTransaction {
        txType: U256::from(REAL_L1_PRIORITY_TX_TYPE_ID),
        from: U256::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
        to: U256::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
        gasLimit: U256::from(10000000000u64),
        gasPerPubdataByteLimit: U256::from(1000),
        maxFeePerGas: U256::from(1),
        maxPriorityFeePerGas: U256::from(0),
        paymaster: U256::ZERO,
        nonce: U256::from(0),
        value: U256::from(100),
        reserved: [
            // `toMint`
            U256::from(100000000000000000000000000000u128),
            // `refundRecipient`
            U256::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
            U256::from(0),
            U256::from(0),
        ],
        data: Bytes::new(),
        signature: Bytes::new(),
        factoryDeps: vec![],
        paymasterInput: Bytes::new(),
        reservedDynamic: Bytes::new(),
    };
    L1Envelope::try_from(tx).expect("forced deposit transaction is malformed")
}
