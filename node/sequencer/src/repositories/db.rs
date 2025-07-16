use crate::repositories::transaction_receipt_repository::{StoredTxData, TxMeta};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Block, ReceiptEnvelope, Transaction};
use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::TxHash;
use alloy::rlp::{Decodable, Encodable};
use tokio::sync::watch;
use zksync_os_types::{L2Transaction, ZkEnvelope};
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
    latest_block_number: watch::Sender<u64>,
}

impl RepositoryDB {
    pub fn new(db: RocksDB<RepositoryCF>) -> Self {
        let latest_block_number_value = db
            .get_cf(RepositoryCF::Meta, RepositoryCF::block_number_key())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .unwrap_or(0);

        Self {
            db,
            latest_block_number: watch::channel(latest_block_number_value).0,
        }
    }

    pub fn latest_block_number(&self) -> u64 {
        *self.latest_block_number.borrow()
    }

    /// Waits until the latest block number is at least `block_number`.
    /// Returns the latest block number once it is reached.
    pub async fn wait_for_block_number(&self, block_number: u64) -> u64 {
        *self
            .latest_block_number
            .subscribe()
            .wait_for(|value| *value >= block_number)
            .await
            .unwrap()
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

    pub fn get_stored_tx_by_hash(&self, hash: TxHash) -> Option<StoredTxData> {
        let tx_bytes = self.db.get_cf(RepositoryCF::Tx, &hash.0).unwrap()?;
        let receipt_bytes = self.db.get_cf(RepositoryCF::TxReceipt, &hash.0).unwrap()?;
        let meta_bytes = self.db.get_cf(RepositoryCF::TxMeta, &hash.0).unwrap()?;

        let tx_envelope = ZkEnvelope::decode_2718(&mut tx_bytes.as_slice()).unwrap();
        let tx = match tx_envelope {
            ZkEnvelope::L1(l1_envelope) => l1_envelope.into(),
            ZkEnvelope::L2(l2_envelope) => {
                let signer = l2_envelope.recover_signer().unwrap();
                L2Transaction::new_unchecked(l2_envelope, signer).into()
            }
        };
        let receipt = ReceiptEnvelope::decode_2718(&mut receipt_bytes.as_slice()).unwrap();
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
        self.latest_block_number.send_replace(block_number);
    }

    fn add_tx_to_write_batch(batch: &mut WriteBatch<RepositoryCF>, tx: &StoredTxData) {
        let tx_hash = tx.tx.hash();
        let mut tx_bytes = Vec::new();
        tx.tx.inner.encode_2718(&mut tx_bytes);
        batch.put_cf(RepositoryCF::Tx, tx_hash.as_slice(), &tx_bytes);

        let mut receipt_bytes = Vec::new();
        tx.receipt.encode_2718(&mut receipt_bytes);
        batch.put_cf(RepositoryCF::TxReceipt, tx_hash.as_slice(), &receipt_bytes);

        let mut tx_meta_bytes = Vec::new();
        tx.meta.encode(&mut tx_meta_bytes);
        batch.put_cf(RepositoryCF::TxMeta, tx_hash.as_slice(), &tx_meta_bytes);

        let initiator = tx.tx.signer();
        let nonce = tx.tx.inner.nonce();
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
