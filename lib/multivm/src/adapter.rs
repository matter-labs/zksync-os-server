use alloy::consensus::transaction::Recovered;
use alloy::eips::Decodable2718;
use zksync_os_interface::traits::{EncodedTx, NextTxResponse, TxSource};
use zksync_os_types::{TransactionData, ZkEnvelope, ZkTransaction};

pub(crate) fn convert_tx_to_abi(encoded_tx: EncodedTx) -> EncodedTx {
    match encoded_tx {
        EncodedTx::Abi(b) => EncodedTx::Abi(b),
        EncodedTx::Rlp(rlp_bytes, signer) => {
            let envelope = ZkEnvelope::decode_2718(&mut rlp_bytes.as_slice())
                .expect("Failed to decode 2718 transaction");
            let tx = ZkTransaction {
                inner: Recovered::new_unchecked(envelope, signer),
            };
            EncodedTx::Abi(TransactionData::from(tx).abi_encode())
        }
    }
}

#[derive(Debug)]
pub struct AbiTxSource<T> {
    inner: T,
}

impl<T> AbiTxSource<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: TxSource> TxSource for AbiTxSource<T> {
    fn get_next_tx(&mut self) -> NextTxResponse {
        let r = self.inner.get_next_tx();
        match r {
            NextTxResponse::SealBlock => NextTxResponse::SealBlock,
            NextTxResponse::Tx(tx) => NextTxResponse::Tx(convert_tx_to_abi(tx)),
        }
    }
}
