use crate::repositories::{
    api_interface::RepositoryBlock,
    metrics::REPOSITORIES_METRICS,
    transaction_receipt_repository::{StoredTxData, TxMeta},
};
use alloy::{
    consensus::{Block, ReceiptEnvelope, Transaction, transaction::SignerRecoverable},
    eips::{Decodable2718, Encodable2718},
    primitives::{Address, BlockHash, BlockNumber, TxHash, TxNonce},
    rlp::{Decodable, Encodable},
};
use tokio::sync::watch;
use zksync_os_types::{L2Transaction, ZkEnvelope};
use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::{RocksDB, rocksdb};

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

    pub fn get_block_by_number(&self, number: BlockNumber) -> DbResult<Option<RepositoryBlock>> {
        let block_number_bytes = number.to_be_bytes();
        let Some(block_hash_bytes) = self
            .db
            .get_cf(RepositoryCF::BlockNumberToHash, &block_number_bytes)?
        else {
            return Ok(None);
        };
        let hash = BlockHash::from(
            <[u8; 32]>::try_from(block_hash_bytes).expect("block hash must be 32 bytes long"),
        );
        self.get_block_by_hash(hash)
    }

    pub fn get_block_by_hash(&self, hash: BlockHash) -> DbResult<Option<RepositoryBlock>> {
        let Some(bytes) = self.db.get_cf(RepositoryCF::BlockData, hash.as_slice())? else {
            return Ok(None);
        };
        let block = Block::decode(&mut bytes.as_slice())?;
        Ok(Some(RepositoryBlock::new_unchecked(block, hash)))
    }

    pub fn get_stored_tx_by_hash(&self, hash: TxHash) -> DbResult<Option<StoredTxData>> {
        let Some(tx_bytes) = self.db.get_cf(RepositoryCF::Tx, &hash.0)? else {
            return Ok(None);
        };
        let Some(receipt_bytes) = self.db.get_cf(RepositoryCF::TxReceipt, &hash.0)? else {
            return Ok(None);
        };
        let Some(meta_bytes) = self.db.get_cf(RepositoryCF::TxMeta, &hash.0)? else {
            return Ok(None);
        };

        let tx_envelope = ZkEnvelope::decode_2718(&mut tx_bytes.as_slice())?;
        let tx = match tx_envelope {
            ZkEnvelope::L1(l1_envelope) => l1_envelope.into(),
            ZkEnvelope::L2(l2_envelope) => {
                let signer = l2_envelope
                    .recover_signer()
                    .expect("transaction saved in DB is not EC recoverable");
                L2Transaction::new_unchecked(l2_envelope, signer).into()
            }
        };
        let receipt = ReceiptEnvelope::decode_2718(&mut receipt_bytes.as_slice())?;
        let meta = TxMeta::decode(&mut meta_bytes.as_slice())?;

        Ok(Some(StoredTxData { tx, receipt, meta }))
    }

    pub fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> DbResult<Option<TxHash>> {
        let mut sender_and_nonce_key = Vec::with_capacity(20 + 8);
        sender_and_nonce_key.extend_from_slice(sender.as_slice());
        sender_and_nonce_key.extend_from_slice(&nonce.to_be_bytes());
        let Some(tx_hash_bytes) = self.db.get_cf(
            RepositoryCF::InitiatorAndNonceToHash,
            sender_and_nonce_key.as_slice(),
        )?
        else {
            return Ok(None);
        };
        let tx_hash = TxHash::from(
            <[u8; 32]>::try_from(tx_hash_bytes).expect("tx hash must be 32 bytes long"),
        );
        Ok(Some(tx_hash))
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

        REPOSITORIES_METRICS
            .block_data_size
            .observe(batch.size_in_bytes());
        REPOSITORIES_METRICS
            .block_data_size_per_tx
            .observe(batch.size_in_bytes() / txs.len());
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

/// DB result type.
pub type DbResult<Ok> = Result<Ok, DbError>;

/// Error variants thrown by various repositories.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Rocksdb(#[from] rocksdb::Error),
    #[error(transparent)]
    Eip2718(#[from] alloy::eips::eip2718::Eip2718Error),
    #[error(transparent)]
    Rlp(#[from] alloy::rlp::Error),
}
