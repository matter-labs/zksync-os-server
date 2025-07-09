use crate::repositories::transaction_receipt_repository::TransactionApiData;
use alloy::consensus::Transaction;
use alloy::primitives::TxHash;
use alloy::rpc::types::Header;
use zksync_storage::db::NamedColumnFamily;
use zksync_storage::RocksDB;

#[derive(Clone, Copy, Debug)]
pub enum RepositoryCF {
    // block hash => (block header, array of tx hashes)
    BlockData,
    // block number => block hash
    BlockNumberToHash,
    // tx hash => (tx receipt, tx envelop)
    TxData,
    // (initiator address, nonce) => tx hash
    InitiatorAndNonceToHash,
    // meta fields: currently only latest block number
    Meta,
}

impl RepositoryCF {
    fn block_number_key() -> &'static [u8] {
        b"block_number"
    }
}

impl NamedColumnFamily for RepositoryCF {
    const DB_NAME: &'static str = "repository";
    const ALL: &'static [Self] = &[
        RepositoryCF::BlockData,
        RepositoryCF::BlockNumberToHash,
        RepositoryCF::TxData,
        RepositoryCF::InitiatorAndNonceToHash,
        RepositoryCF::Meta,
    ];

    fn name(&self) -> &'static str {
        match self {
            RepositoryCF::BlockData => "block_data",
            RepositoryCF::BlockNumberToHash => "block_number_to_hash",
            RepositoryCF::TxData => "tx_data",
            RepositoryCF::InitiatorAndNonceToHash => "initiator_and_nonce_to_hash",
            RepositoryCF::Meta => "meta",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepositoryDB {
    db: RocksDB<RepositoryCF>,
}

impl RepositoryDB {
    pub fn new(db: RocksDB<RepositoryCF>) -> Self {
        Self { db }
    }

    pub fn latest_block_number(&self) -> u64 {
        self.db
            .get_cf(RepositoryCF::Meta, RepositoryCF::block_number_key())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .unwrap_or(0)
    }

    pub fn get_block_by_number(&self, number: u64) -> Option<(Header, Vec<TxHash>)> {
        let block_number_bytes = number.to_be_bytes();
        let block_hash_bytes = self
            .db
            .get_cf(RepositoryCF::BlockNumberToHash, &block_number_bytes)
            .unwrap()?;
        let bytes = self
            .db
            .get_cf(RepositoryCF::BlockData, &block_hash_bytes)
            .unwrap();
        bytes.map(|bytes| {
            let header_len = u64::from_be_bytes(bytes[0..8].to_vec().try_into().unwrap()) as usize;
            let header = serde_json::from_slice(&bytes[8..(8 + header_len)]).unwrap();
            // let header = bincode::serde::decode_from_slice(&bytes[8..(8 + header_len)], bincode::config::standard()).unwrap().0;
            // let header = ciborium::from_reader(&bytes[8..(8 + header_len)]).unwrap();

            let bytes_left = bytes.len() - (8 + header_len);
            assert_eq!(bytes_left % 32, 0, "Invalid block data length");
            let number_of_hashes = bytes_left / 32;
            let tx_hashes = (0..number_of_hashes)
                .map(|i| {
                    TxHash::from_slice(
                        &bytes[(8 + header_len + i * 32)..(8 + header_len + (i + 1) * 32)],
                    )
                })
                .collect();

            (header, tx_hashes)
        })
    }

    pub fn get_tx_by_hash(&self, hash: TxHash) -> Option<TransactionApiData> {
        let bytes = self.db.get_cf(RepositoryCF::TxData, &hash.0).unwrap();
        bytes.map(|bytes| {
            serde_json::from_slice(&bytes).unwrap()
            // bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap().0
            // ciborium::from_reader(&bytes[..]).unwrap()
        })
    }

    pub fn write_block(&self, header: &Header, txs: &[TransactionApiData]) {
        let block_number = header.number;
        let block_hash = header.hash.0;
        let block_number_bytes = block_number.to_be_bytes();
        let block_hash_bytes = block_hash.to_vec();

        let mut batch = self.db.new_write_batch();
        batch.put_cf(
            RepositoryCF::BlockNumberToHash,
            &block_number_bytes,
            &block_hash_bytes,
        );

        let header_bytes = serde_json::to_vec(&header).unwrap();
        // let header_bytes = bincode::serde::encode_to_vec(&header, bincode::config::standard()).unwrap();
        // let mut header_bytes = Vec::new();
        // ciborium::into_writer(header, &mut header_bytes).unwrap();
        let mut block_data = Vec::with_capacity(8 + header_bytes.len() + txs.len() * 32);
        block_data.extend_from_slice(&(header_bytes.len() as u64).to_be_bytes());
        block_data.extend_from_slice(&header_bytes);
        for tx in txs {
            block_data.extend_from_slice(&tx.receipt.transaction_hash.0);
        }
        batch.put_cf(RepositoryCF::BlockData, &block_hash, &block_data);

        for tx in txs {
            let tx_hash = tx.receipt.transaction_hash.0;
            let tx_data_bytes = serde_json::to_vec(tx).unwrap();
            // let tx_data_bytes = bincode::serde::encode_to_vec(tx, bincode::config::standard()).unwrap();
            // let mut tx_data_bytes = Vec::new();
            // ciborium::into_writer(tx, &mut tx_data_bytes).unwrap();
            batch.put_cf(RepositoryCF::TxData, &tx_hash, &tx_data_bytes);

            let initiator = tx.receipt.from.0;
            let nonce = tx.transaction.nonce();
            let mut initiator_and_nonce_key = Vec::with_capacity(20 + 8);
            initiator_and_nonce_key.extend_from_slice(initiator.as_slice());
            initiator_and_nonce_key.extend_from_slice(&nonce.to_be_bytes());
            batch.put_cf(
                RepositoryCF::InitiatorAndNonceToHash,
                &initiator_and_nonce_key,
                &tx_hash,
            );
        }

        let block_number_key = RepositoryCF::block_number_key();
        batch.put_cf(RepositoryCF::Meta, block_number_key, &block_number_bytes);

        self.db.write(batch).unwrap();
    }
}
