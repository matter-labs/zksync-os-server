use alloy::consensus::Sealed;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, B256, BlockHash, TxHash, U64, U256};
use alloy::rpc::types::{FeeHistory, Log};
use anyhow::Context;
use blake2::{Blake2s256, Digest};
use jsonrpsee::core::Serialize;
use serde::Deserialize;
use zksync_os_merkle_tree_api::flat;
use zksync_os_types::{BlockExt, ZkEnvelope, ZkReceiptEnvelope};

pub type ZkTransactionReceipt =
    alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<Log, L2ToL1Log>>;
pub type ZkHeader = alloy::rpc::types::Header;

pub type ZkApiTransaction = alloy::rpc::types::Transaction<ZkEnvelope>;

pub type ZkApiBlock = alloy::rpc::types::Block<ZkApiTransaction>;

pub trait RpcBlockConvert {
    fn into_rpc(self) -> ZkApiBlock;
}

impl RpcBlockConvert for Sealed<alloy::consensus::Block<TxHash>> {
    fn into_rpc(self) -> ZkApiBlock {
        let hash = self.hash();
        let block = self.unseal();
        let rlp_length = block.rlp_length();
        ZkApiBlock::new(
            ZkHeader::from_consensus(
                block.header.seal(hash),
                Some(U256::ZERO),
                Some(U256::from(rlp_length)),
            ),
            BlockTransactions::Hashes(block.body.transactions),
        )
    }
}

/// A struct with the proof for the L2->L1 log in a specific block.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct L2ToL1LogProof {
    /// The L1 batch number containing the log.
    pub batch_number: u64,
    /// The merkle path for the leaf.
    pub proof: Vec<B256>,
    /// The id of the leaf in a tree.
    pub id: u32,
    /// The root of the tree.
    pub root: B256,
}

/// Selects the root that the returned merkle proof anchors to.
///
/// If omitted from the RPC call, [`LogProofTarget::L1BatchRoot`] is used.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogProofTarget {
    /// Proof anchored to the SL L1 batch aggregated root.
    ///
    /// The proof covers the full gateway batch range and includes the local-root extension,
    /// making it suitable for L1 verification.
    #[default]
    L1BatchRoot,
    /// Proof anchored to the SL block-level message root.
    ///
    /// The proof targets the specific execution block (no local-root extension),
    /// making it suitable for cross-chain interop message verification.
    MessageRoot,
}

/// ZKsync-specific block metadata struct.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockMetadata {
    pub pubdata_price_per_byte: U256,
    pub native_price: U256,
    pub execution_version: u32,
}

/// Extended FeeHistory struct including L2 pubdata price history.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct L2FeeHistory {
    #[serde(flatten)]
    pub base: FeeHistory,
    pub pubdata_price_per_byte: Option<Vec<U256>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2ToL1Log {
    /// Hash of the block the transaction that emitted this log was mined in
    pub block_hash: Option<BlockHash>,
    /// Number of the block the transaction that emitted this log was mined in
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_number: Option<u64>,
    /// The timestamp of the block.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_timestamp: Option<u64>,
    /// Transaction Hash
    #[doc(alias = "tx_hash")]
    pub transaction_hash: Option<TxHash>,
    /// Index of the Transaction in the block
    #[serde(with = "alloy::serde::quantity::opt")]
    #[doc(alias = "tx_index")]
    pub transaction_index: Option<u64>,
    /// Log Index in Block
    #[serde(with = "alloy::serde::quantity::opt")]
    pub log_index: Option<u64>,
    /// Log Index in Transaction, needed for compatibility with ZKSync Era L2->L1 log format.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub transaction_log_index: Option<u64>,
    /// Deprecated, kept for compatibility, always set to 0.
    #[serde(with = "alloy::serde::quantity")]
    pub shard_id: u64,
    /// Deprecated, kept for compatibility, always set to `true`.
    pub is_service: bool,
    /// The L2 address which sent the log.
    /// For user messages set to `L1Messenger` system hook address,
    /// for l1 -> l2 txs logs - `BootloaderFormalAddress`.
    pub sender: Address,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message sender address(padded),
    /// for l1 -> l2 txs logs - transaction hash.
    pub key: B256,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message hash.
    /// for l1 -> l2 txs logs - success flag(padded).
    pub value: B256,
}

impl From<L2ToL1Log> for zksync_os_types::L2ToL1Log {
    fn from(value: L2ToL1Log) -> Self {
        Self {
            l2_shard_id: value.shard_id as u8,
            is_service: value.is_service,
            tx_number_in_block: value.transaction_index.expect("Missing transaction index") as u16,
            sender: value.sender,
            key: value.key,
            value: value.value,
        }
    }
}

/// Data hashed into the state commitment of the batch together with the Merkle tree root hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateCommitmentPreimage {
    /// Number of leaves in the Merkle tree.
    pub next_free_slot: U64,
    /// Number of the last block in the batch.
    pub block_number: U64,
    /// Linear Blake2s-256 hash of the last 256 block hashes ending with `block_number` (inclusive).
    pub last_256_block_hashes_blake: B256,
    /// Timestamp (in seconds) of the last block in the batch.
    pub last_block_timestamp: U64,
}

impl StateCommitmentPreimage {
    /// Hashes this preimage together with the provided Merkle tree root hash, resulting the state commitment hash
    /// recorded on L1 (accessible e.g. via `BlockCommit` event emitted by the diamond proxy).
    pub fn hash(&self, tree_root_hash: B256) -> B256 {
        let mut hasher = Blake2s256::new();
        hasher.update(tree_root_hash.as_slice());
        hasher.update(self.next_free_slot.to_be_bytes::<8>());
        hasher.update(self.block_number.to_be_bytes::<8>());
        hasher.update(self.last_256_block_hashes_blake);
        hasher.update(self.last_block_timestamp.to_be_bytes::<8>());
        B256::from_slice(&hasher.finalize())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AddressScopedKey(pub B256);

impl AddressScopedKey {
    // There's a similar function in `zk_ee`, but it relies on multiple unstable features.
    fn derive_flat_key(address: Address, key: B256) -> B256 {
        let mut hasher = Blake2s256::new();
        hasher.update([0_u8; 12]); // address padding
        hasher.update(address.0);
        hasher.update(key.0);
        B256::from_slice(&hasher.finalize())
    }

    fn to_flat_key(self, address: Address) -> B256 {
        Self::derive_flat_key(address, self.0)
    }
}

/// Data from `StoredBatchInfo` needed to reconstruct and verify the batch hash against L1.
///
/// Together with the state commitment (derived from the Merkle proof), these fields allow
/// reconstructing the full `StoredBatchInfo` struct and comparing its keccak256 hash against
/// `storedBatchHash(batchNumber)` on the diamond proxy.
///
/// Two `StoredBatchInfo` fields are omitted because they are always zero in ZKsync OS:
/// `indexRepeatedStorageChanges` and `timestamp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L1VerificationData {
    pub batch_number: u64,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub commitment: B256,
}

/// Storage proof returned from the `zks_getProof` RPC method. Rooted in the batch hash recorded on L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchStorageProof {
    /// Queried address (duplicated to make the proof self-sufficient).
    pub address: Address,
    /// State commitment preimage data, excluding the Merkle tree root hash (which can be recovered from
    /// `storage_proofs`).
    pub state_commitment_preimage: StateCommitmentPreimage,
    /// Flat storage proofs for each queried key.
    pub storage_proofs: Vec<flat::StorageSlotProof<AddressScopedKey>>,
    /// Fields from `StoredBatchInfo` for L1 verification.
    pub l1_verification_data: L1VerificationData,
}

impl BatchStorageProof {
    const TREE_DEPTH: u8 = 64;

    /// Verifies this proof.
    ///
    /// # Panics
    ///
    /// Panics if `queried_keys` is empty (the proof would be useless in this case).
    pub fn verify(
        &self,
        queried_address: Address,
        queried_keys: &[B256],
    ) -> anyhow::Result<StorageView> {
        assert!(!queried_keys.is_empty(), "useless proof");

        anyhow::ensure!(
            self.address == queried_address,
            "Mismatched address: queried {queried_address:?}, got {:?}",
            self.address
        );
        let actual_keys = self.storage_proofs.iter().map(|proof| proof.key.0);
        anyhow::ensure!(
            actual_keys.clone().eq(queried_keys.iter().copied()),
            "Mismatched proven slots: queried {queried_keys:?}, got {:?}",
            actual_keys.collect::<Vec<_>>()
        );

        let mut cached_tree_root_hash = None;
        let mut storage_values = Vec::with_capacity(self.storage_proofs.len());
        for proof in &self.storage_proofs {
            let flat_key = proof.key.to_flat_key(self.address);
            let tree_root_hash = proof
                .proof
                .verify(Self::TREE_DEPTH, flat_key)
                .with_context(|| format!("invalid proof for key {:?}", proof.key))?;
            if let Some(cached) = cached_tree_root_hash {
                anyhow::ensure!(
                    cached == tree_root_hash,
                    "Tree root hash mismatch for key {:?}: expected {cached:?}, got {tree_root_hash:?}",
                    proof.key,
                );
            } else {
                cached_tree_root_hash = Some(tree_root_hash);
            }

            storage_values.push(proof.value());
        }

        // `unwrap()` is safe due to checks above.
        let tree_root_hash = cached_tree_root_hash.unwrap();
        Ok(StorageView {
            storage_commitment: self.state_commitment_preimage.hash(tree_root_hash),
            storage_values,
        })
    }
}

/// Proven view of the storage returned from [`BatchStorageProof::verify()`].
#[derive(Debug)]
pub struct StorageView {
    /// Storage commitment hash. In most cases, must be checked against L1.
    pub storage_commitment: B256,
    /// Proven storage values in the order of queried keys.
    pub storage_values: Vec<Option<B256>>,
}
