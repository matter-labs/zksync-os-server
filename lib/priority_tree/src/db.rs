use crate::PriorityOpsLeaf;
use alloy::primitives::B256;
use bincode::{Decode, Encode};
use std::path::Path;
use zksync_os_crypto::hasher::keccak::KeccakHasher;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

#[derive(Debug, Encode, Decode)]
pub struct CachedTreeData {
    /// Index of the leftmost untrimmed leaf.
    pub start_index: usize,
    /// Left subset of the Merkle path to the first untrimmed leaf
    #[bincode(with_serde)]
    pub cache: Vec<Option<B256>>,
}

#[derive(Clone, Copy, Debug)]
pub enum PriorityTreeCF {
    CachedTreeData,
}

impl PriorityTreeCF {
    fn block_number_key() -> &'static [u8] {
        b"block_number"
    }

    fn data_key() -> &'static [u8] {
        b"data"
    }
}

impl NamedColumnFamily for PriorityTreeCF {
    const DB_NAME: &'static str = "priority_tree";
    const ALL: &'static [Self] = &[PriorityTreeCF::CachedTreeData];

    fn name(&self) -> &'static str {
        match self {
            PriorityTreeCF::CachedTreeData => "cached_tree_data",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PriorityTreeDB {
    db: RocksDB<PriorityTreeCF>,
}

impl PriorityTreeDB {
    pub fn new(db_path: &Path) -> Self {
        let db = RocksDB::<PriorityTreeCF>::new(db_path).expect("Failed to open db");
        Self { db }
    }

    /// Initializes the priority tree from the database. Returns the last block number processed and the tree.
    /// If the database is empty, it returns an empty tree and block number 0.
    pub fn init_tree(&self) -> anyhow::Result<(u64, MiniMerkleTree<PriorityOpsLeaf>)> {
        let block_number = self
            .db
            .get_cf(
                PriorityTreeCF::CachedTreeData,
                PriorityTreeCF::block_number_key(),
            )
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()));
        if let Some(block_number) = block_number {
            let cached_data_bytes = self
                .db
                .get_cf(PriorityTreeCF::CachedTreeData, PriorityTreeCF::data_key())?
                .expect("Cached tree data must be present if block number is present");
            let cached_data: CachedTreeData =
                bincode::decode_from_slice(&cached_data_bytes, bincode::config::standard())?.0;
            Ok((
                block_number,
                MiniMerkleTree::init_cached(
                    KeccakHasher,
                    cached_data.start_index,
                    cached_data.cache,
                ),
            ))
        } else {
            Ok((
                0,
                MiniMerkleTree::from_hashes(KeccakHasher, [].into_iter(), None),
            ))
        }
    }

    pub fn cache_tree(
        &self,
        tree: &MiniMerkleTree<PriorityOpsLeaf>,
        block_number: u64,
    ) -> anyhow::Result<()> {
        let cached_data = CachedTreeData {
            start_index: tree.start_index(),
            cache: tree.cache().to_vec(),
        };
        let encoded = bincode::encode_to_vec(cached_data, bincode::config::standard())?;

        let mut write_batch = self.db.new_write_batch();
        write_batch.put_cf(
            PriorityTreeCF::CachedTreeData,
            PriorityTreeCF::block_number_key(),
            &block_number.to_be_bytes(),
        );
        write_batch.put_cf(
            PriorityTreeCF::CachedTreeData,
            PriorityTreeCF::data_key(),
            &encoded,
        );
        self.db.write(write_batch)?;

        Ok(())
    }
}
