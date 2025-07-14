use crate::repositories::transaction_receipt_repository::{StoredTxData, TxMeta};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Block, ReceiptEnvelope, Transaction};
use alloy::primitives::{Address, TxHash};
use alloy::rlp::{Decodable, Encodable};
use zksync_os_types::{L2Envelope, L2Transaction};
use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::RocksDB;

#[derive(Clone, Copy, Debug)]
pub enum RepositoryCF {
    // block hash => (block header, array of tx hashes)
    BlockData,
    // block number => block hash
    BlockNumberToHash,
    // tx hash => tx
    Tx,
    // tx hash => receipt envelope
    TxReceipt,
    // tx hash => tx meta
    TxMeta,
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
        RepositoryCF::Tx,
        RepositoryCF::TxReceipt,
        RepositoryCF::TxMeta,
        RepositoryCF::InitiatorAndNonceToHash,
        RepositoryCF::Meta,
    ];

    fn name(&self) -> &'static str {
        match self {
            RepositoryCF::BlockData => "block_data",
            RepositoryCF::BlockNumberToHash => "block_number_to_hash",
            RepositoryCF::Tx => "tx",
            RepositoryCF::TxReceipt => "tx_receipt",
            RepositoryCF::TxMeta => "tx_meta",
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

    pub fn get_block_by_number(&self, number: u64) -> Option<Block<TxHash>> {
        let block_number_bytes = number.to_be_bytes();
        let block_hash_bytes = self
            .db
            .get_cf(RepositoryCF::BlockNumberToHash, &block_number_bytes)
            .unwrap()?;
        let bytes = self
            .db
            .get_cf(RepositoryCF::BlockData, &block_hash_bytes)
            .unwrap();
        bytes.map(|bytes| Block::decode(&mut bytes.as_slice()).unwrap())
    }

    pub fn get_tx_by_hash(&self, hash: TxHash) -> Option<L2Transaction> {
        let bytes = self.db.get_cf(RepositoryCF::Tx, &hash.0).unwrap();
        bytes.map(|bytes| L2Transaction::decode(&mut bytes.as_slice()).unwrap())
    }

    pub fn get_tx_receipt_by_hash(&self, hash: TxHash) -> Option<ReceiptEnvelope> {
        let bytes = self.db.get_cf(RepositoryCF::TxReceipt, &hash.0).unwrap();
        bytes.map(|bytes| ReceiptEnvelope::decode(&mut bytes.as_slice()).unwrap())
    }

    pub fn get_stored_tx_by_hash(&self, hash: TxHash) -> Option<StoredTxData> {
        let tx_bytes = self.db.get_cf(RepositoryCF::Tx, &hash.0).unwrap()?;
        let receipt_bytes = self.db.get_cf(RepositoryCF::TxReceipt, &hash.0).unwrap()?;
        let meta_bytes = self.db.get_cf(RepositoryCF::TxMeta, &hash.0).unwrap()?;

        let tx_bytes_mut = &mut &mut tx_bytes.as_slice();
        let tx_envelope = L2Envelope::decode(tx_bytes_mut).unwrap();
        let signer = if tx_bytes_mut.is_empty() {
            // l2 tx
            tx_envelope.recover_signer().unwrap()
        } else {
            // l1->l2 tx
            Address::from_slice(tx_bytes_mut)
        };
        let tx = L2Transaction::new_unchecked(tx_envelope, signer);
        let receipt = ReceiptEnvelope::decode(&mut receipt_bytes.as_slice()).unwrap();
        let meta = TxMeta::decode(&mut meta_bytes.as_slice()).unwrap();

        Some(StoredTxData { tx, receipt, meta })
    }

    pub fn write_block(&self, block: &Block<TxHash>, txs: &[StoredTxData]) {
        let block_number = block.number;
        let block_hash = block.hash_slow();
        let block_number_bytes = block_number.to_be_bytes();
        let block_hash_bytes = block_hash.to_vec();

        let mut batch = self.db.new_write_batch();
        batch.put_cf(
            RepositoryCF::BlockNumberToHash,
            &block_number_bytes,
            &block_hash_bytes,
        );

        let mut block_bytes = Vec::new();
        block.encode(&mut block_bytes);
        batch.put_cf(RepositoryCF::BlockData, block_hash.as_slice(), &block_bytes);

        for tx in txs {
            Self::add_tx_to_write_batch(&mut batch, tx);
        }

        let block_number_key = RepositoryCF::block_number_key();
        batch.put_cf(RepositoryCF::Meta, block_number_key, &block_number_bytes);

        self.db.write(batch).unwrap();
    }

    fn add_tx_to_write_batch(batch: &mut WriteBatch<RepositoryCF>, tx: &StoredTxData) {
        let tx_hash = tx.tx.hash();
        let mut tx_bytes = Vec::new();
        tx.tx.encode(&mut tx_bytes);
        // Kludge for L1 txs.
        if tx.tx.signature().r().is_zero() {
            tx_bytes.extend_from_slice(tx.tx.signer().as_slice());
        }
        batch.put_cf(RepositoryCF::Tx, tx_hash.as_slice(), &tx_bytes);

        let mut receipt_bytes = Vec::new();
        tx.receipt.encode(&mut receipt_bytes);
        batch.put_cf(RepositoryCF::TxReceipt, tx_hash.as_slice(), &receipt_bytes);

        let mut tx_meta_bytes = Vec::new();
        tx.meta.encode(&mut tx_meta_bytes);
        batch.put_cf(RepositoryCF::TxMeta, tx_hash.as_slice(), &tx_meta_bytes);

        let initiator = tx.tx.signer();
        let nonce = tx.tx.nonce();
        let mut initiator_and_nonce_key = Vec::with_capacity(20 + 8);
        initiator_and_nonce_key.extend_from_slice(initiator.as_slice());
        initiator_and_nonce_key.extend_from_slice(&nonce.to_be_bytes());
        batch.put_cf(
            RepositoryCF::InitiatorAndNonceToHash,
            &initiator_and_nonce_key,
            tx_hash.as_slice(),
        );
    }
}
