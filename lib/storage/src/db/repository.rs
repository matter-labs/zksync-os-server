use crate::metrics::REPOSITORIES_METRICS;
use alloy::consensus::Sealed;
use alloy::{
    consensus::{Block, Transaction},
    eips::{Decodable2718, Encodable2718},
    primitives::{Address, BlockHash, BlockNumber, TxHash, TxNonce},
    rlp::{Decodable, Encodable},
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;
use zksync_os_genesis::Genesis;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_storage_api::{
    ReadRepository, RepositoryBlock, RepositoryResult, StoredTxData, TxMeta,
};
use zksync_os_types::{ZkEnvelope, ZkReceiptEnvelope, ZkTransaction};

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
pub struct RepositoryDb {
    db: RocksDB<RepositoryCF>,
    /// Points to the latest block whose data has been persisted in `db`. There might be partial
    /// data written for the next block, in other words `db` is caught up to *AT LEAST* this number.
    latest_block_number: watch::Sender<u64>,
}

impl RepositoryDb {
    pub async fn new(db_path: &Path, genesis: &Genesis) -> Self {
        let db = RocksDB::<RepositoryCF>::new(db_path).expect("Failed to open db");
        let latest_block_number = db
            .get_cf(RepositoryCF::Meta, RepositoryCF::block_number_key())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()));
        let latest_block_number = if let Some(n) = latest_block_number {
            n
        } else {
            let header = genesis.state().await.header.clone();
            let hash = header.hash_slow();
            let block = Sealed::new_unchecked(
                Block {
                    header,
                    body: alloy::consensus::BlockBody {
                        transactions: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                },
                hash,
            );
            Self::write_block_inner(&db, &block, &[]);

            0
        };

        Self {
            db,
            latest_block_number: watch::channel(latest_block_number).0,
        }
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

    fn write_block_inner(
        db: &RocksDB<RepositoryCF>,
        block: &Sealed<Block<TxHash>>,
        txs: &[Arc<StoredTxData>],
    ) {
        let block_number = block.number;
        let block_hash = block.hash();
        let block_number_bytes = block_number.to_be_bytes();
        let block_hash_bytes = block_hash.to_vec();

        let mut batch = db.new_write_batch();
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
            .observe(batch.size_in_bytes() / txs.len().max(1));
        db.write(batch).unwrap();
    }

    pub fn write_block(&self, block: &Sealed<Block<TxHash>>, txs: &[Arc<StoredTxData>]) {
        Self::write_block_inner(&self.db, block, txs);
        self.latest_block_number.send_replace(block.number);
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

    pub fn rollback(&self, last_block_to_keep: u64) -> RepositoryResult<()> {
        let latest_block_number = self
            .db
            .get_cf(RepositoryCF::Meta, RepositoryCF::block_number_key())?
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .expect("latest block number must be present in DB");
        if latest_block_number > last_block_to_keep {
            tracing::info!(
                "Rolling back repository DB blocks [{}; {}]",
                last_block_to_keep + 1,
                latest_block_number
            );
            let mut batch = self.db.new_write_batch();
            let last_block_to_keep_bytes = last_block_to_keep.to_be_bytes();
            let block_number_key = RepositoryCF::block_number_key();
            batch.put_cf(
                RepositoryCF::Meta,
                block_number_key,
                &last_block_to_keep_bytes,
            );

            for block_number in (last_block_to_keep + 1)..=latest_block_number {
                let old_repo_block = self
                    .get_block_by_number(block_number)?
                    .expect("block to rollback must be present in DB");
                let block_number_bytes = block_number.to_be_bytes();
                batch.delete_cf(RepositoryCF::BlockNumberToHash, &block_number_bytes);
                batch.delete_cf(RepositoryCF::BlockData, &old_repo_block.hash().0);

                for tx_hash in &old_repo_block.body.transactions {
                    batch.delete_cf(RepositoryCF::Tx, &tx_hash.0);
                    batch.delete_cf(RepositoryCF::TxReceipt, &tx_hash.0);
                    batch.delete_cf(RepositoryCF::TxMeta, &tx_hash.0);

                    let tx = self
                        .get_transaction(*tx_hash)?
                        .expect("tx to rollback must be present in DB");
                    let initiator = tx.signer();
                    let nonce = tx.inner.nonce();
                    let mut initiator_and_nonce_key = Vec::with_capacity(20 + 8);
                    initiator_and_nonce_key.extend_from_slice(initiator.as_slice());
                    initiator_and_nonce_key.extend_from_slice(&nonce.to_be_bytes());
                    batch.delete_cf(
                        RepositoryCF::InitiatorAndNonceToHash,
                        &initiator_and_nonce_key,
                    );
                }
            }

            self.db.write(batch)?;
            self.latest_block_number.send_replace(last_block_to_keep);
        }

        Ok(())
    }
}

impl ReadRepository for RepositoryDb {
    fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
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

    fn get_block_by_hash(&self, hash: BlockHash) -> RepositoryResult<Option<RepositoryBlock>> {
        let Some(bytes) = self.db.get_cf(RepositoryCF::BlockData, hash.as_slice())? else {
            return Ok(None);
        };
        let block = Block::decode(&mut bytes.as_slice())?;
        Ok(Some(RepositoryBlock::new_unchecked(block, hash)))
    }

    fn get_raw_transaction(&self, hash: TxHash) -> RepositoryResult<Option<Vec<u8>>> {
        Ok(self.db.get_cf(RepositoryCF::Tx, &hash.0)?)
    }

    fn get_transaction(&self, hash: TxHash) -> RepositoryResult<Option<ZkTransaction>> {
        let Some(tx_bytes) = self.db.get_cf(RepositoryCF::Tx, &hash.0)? else {
            return Ok(None);
        };
        let tx = ZkEnvelope::decode_2718(&mut tx_bytes.as_slice())?
            .try_into_recovered()
            .expect("transaction saved in DB is not EC recoverable");
        Ok(Some(tx))
    }

    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ZkReceiptEnvelope>> {
        let Some(receipt_bytes) = self.db.get_cf(RepositoryCF::TxReceipt, &hash.0)? else {
            return Ok(None);
        };
        let receipt = ZkReceiptEnvelope::decode_2718(&mut receipt_bytes.as_slice())?;
        Ok(Some(receipt))
    }

    fn get_transaction_meta(&self, hash: TxHash) -> RepositoryResult<Option<TxMeta>> {
        let Some(meta_bytes) = self.db.get_cf(RepositoryCF::TxMeta, &hash.0)? else {
            return Ok(None);
        };
        let meta = TxMeta::decode(&mut meta_bytes.as_slice())?;
        Ok(Some(meta))
    }

    fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> RepositoryResult<Option<TxHash>> {
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

    fn get_stored_transaction(&self, hash: TxHash) -> RepositoryResult<Option<StoredTxData>> {
        let Some(tx) = self.get_transaction(hash)? else {
            return Ok(None);
        };
        let Some(receipt) = self.get_transaction_receipt(hash)? else {
            return Ok(None);
        };
        let Some(meta) = self.get_transaction_meta(hash)? else {
            return Ok(None);
        };
        Ok(Some(StoredTxData { tx, receipt, meta }))
    }

    fn get_latest_block(&self) -> u64 {
        *self.latest_block_number.borrow()
    }
}
