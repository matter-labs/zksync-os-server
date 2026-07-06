//! [`ExecutionEnv`] backed by the real zksync-os VM over in-memory state.
//!
//! The state model mirrors what the node will need for consensus: a committed chain
//! (here: a linked list of per-block state layers over the shared genesis) plus a small
//! set of *pending* layers for blocks that consensus is still deciding on. Building and
//! verifying execute against the parent's layer without touching committed state;
//! commit adopts the winning block's layer (re-executing it if the layer is not at
//! hand, e.g. after a restart or when the block arrived via backfill).

use crate::execution::SimEnv;
use crate::stf::block::StfBlock;
use crate::stf::genesis::{SharedGenesis, shared_genesis};
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::primitives::{Address, B256, TxKind, U256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use commonware_consensus::types::Height;
use commonware_cryptography::Digestible;
use commonware_cryptography::sha256::Digest;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tracing::warn;
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{NoopTxCallback, PreimageSource, ReadStorage, TxListSource};
use zksync_os_storage_api::{BlockContext, ViewState};
use zksync_os_types::{
    BlockOutput, L2Envelope, L2Transaction, ZkTransaction, ZksyncOsEncode, hash_block_output,
};

/// Well-known development key; the funded sender of every simulated transaction.
const SENDER_KEY: &str = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// Receives one transfer per block.
pub const TEST_RECIPIENT: Address =
    alloy::primitives::address!("5e6D086F5eC079ADFF4FB3774CDf3e8D6a34F7E9");

/// Address of the funded test sender.
pub fn test_sender_address() -> Address {
    PrivateKeySigner::from_str(SENDER_KEY)
        .expect("valid dev key")
        .address()
}

/// One block's worth of state on top of its parent: the storage writes and preimages
/// that executing the block produced, plus the rolling block-hash ring *after* it.
/// Reads walk the layer chain down to genesis.
struct StateLayer {
    parent: Option<Arc<StateLayer>>,
    storage: HashMap<B256, B256>,
    preimages: HashMap<B256, Vec<u8>>,
    /// The execution context a *child* of this layer must be built with: block hashes
    /// ring already including this block, next block number, next timestamp floor.
    child_context_seed: ContextSeed,
}

#[derive(Clone)]
struct ContextSeed {
    block_hashes: zksync_os_storage_api::BlockHashes,
    next_block_number: u64,
}

/// A read view over a layer chain + the shared genesis. Cheap to clone; the VM takes it
/// by value (twice — storage and preimages).
#[derive(Clone)]
struct LayerView {
    genesis: Arc<SharedGenesis>,
    layer: Option<Arc<StateLayer>>,
}

impl ReadStorage for LayerView {
    fn read(&mut self, key: B256) -> Option<B256> {
        let mut layer = self.layer.as_ref();
        while let Some(current) = layer {
            if let Some(value) = current.storage.get(&key) {
                return Some(*value);
            }
            layer = current.parent.as_ref();
        }
        self.genesis.storage.get(&key).copied()
    }
}

impl PreimageSource for LayerView {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        let mut layer = self.layer.as_ref();
        while let Some(current) = layer {
            if let Some(preimage) = current.preimages.get(&hash) {
                return Some(preimage.clone());
            }
            layer = current.parent.as_ref();
        }
        self.genesis.preimages.get(&hash).cloned()
    }
}

/// Shared-state real execution: clones observe and mutate the same chain, exactly like
/// clones of a real execution handle would.
#[derive(Clone)]
pub struct RealStfExecution {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    genesis: Arc<SharedGenesis>,
    genesis_block: StfBlock,
    /// Height the chain is anchored at: 0 when consensus runs from the true genesis,
    /// the cutover height when it took over pre-existing history. The pre-history
    /// itself lives in `committed_layer` (the state at the anchor) — blocks below
    /// the anchor are never consensus's business.
    anchor_height: u64,
    /// State at the anchor — what the genesis block stands for. `None` when the
    /// anchor is the true genesis state itself.
    anchor_layer: Option<Arc<StateLayer>>,
    /// The committed consensus-era chain (entry `i` has height `anchor_height+i+1`)
    /// and the state layer at the overall tip.
    committed: Vec<StfBlock>,
    committed_layer: Option<Arc<StateLayer>>,
    /// State layers of blocks consensus is still deciding on, keyed by block digest.
    /// Entries at or below the committed height are pruned — they can never be a
    /// parent again.
    pending: HashMap<Digest, Arc<StateLayer>>,
    sender: PrivateKeySigner,
}

impl Default for RealStfExecution {
    fn default() -> Self {
        Self::new()
    }
}

impl RealStfExecution {
    pub fn new() -> Self {
        // Fund the sender generously; one transfer per block spends value + gas.
        let sender = PrivateKeySigner::from_str(SENDER_KEY).expect("valid dev key");
        let genesis = shared_genesis(&[(sender.address(), U256::from(10u128.pow(24)))]);
        let genesis_block = StfBlock::genesis(genesis.header_hash);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                genesis,
                genesis_block,
                anchor_height: 0,
                anchor_layer: None,
                committed: Vec::new(),
                committed_layer: None,
                pending: HashMap::new(),
                sender,
            })),
        }
    }

    /// A chain with `pre_blocks` blocks of real pre-consensus history: the same
    /// one-transfer-per-block schedule the consensus-era builder produces, executed
    /// directly (no consensus involved — this *is* the single-sequencer era), with
    /// consensus then anchored at the resulting tip. Transfer amounts encode
    /// absolute heights, so the recipient-balance formula spans both eras — a
    /// balance check after migration proves the pre-history carried over.
    pub fn anchored(pre_blocks: u64) -> Self {
        let this = Self::new();
        {
            let mut inner = this.inner.lock().unwrap();
            let mut layer = None;
            let mut tip = (0, inner.genesis.header_hash);
            for height in 1..=pre_blocks {
                let timestamp = inner.genesis.context.timestamp + height;
                let transfer = make_transfer(
                    &inner.sender,
                    inner.genesis.context.chain_id,
                    height - 1,
                    height,
                );
                let (output, next_layer) = inner
                    .execute(&layer, timestamp, std::slice::from_ref(&transfer))
                    .expect("pre-consensus history must execute");
                tip = (timestamp, output.header.hash());
                layer = Some(next_layer);
            }
            inner.genesis_block = StfBlock::anchor(pre_blocks, tip.0, tip.1);
            inner.anchor_height = pre_blocks;
            inner.anchor_layer = layer.clone();
            inner.committed_layer = layer;
        }
        this
    }

    /// Balance of `address` in the committed state (test probe).
    pub fn committed_balance(&self, address: Address) -> U256 {
        let inner = self.inner.lock().unwrap();
        inner
            .view_of(inner.committed_layer.clone())
            .balance(address)
    }

    /// Nonce of `address` in the committed state (test probe).
    pub fn committed_nonce(&self, address: Address) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        inner.view_of(inner.committed_layer.clone()).nonce(address)
    }
}

impl Inner {
    fn view_of(&self, layer: Option<Arc<StateLayer>>) -> LayerView {
        LayerView {
            genesis: self.genesis.clone(),
            layer,
        }
    }

    /// The state layer a block's children execute on: the committed tip, one of the
    /// pending candidates, or the genesis itself.
    fn layer_for_parent(&self, parent: &StfBlock) -> Option<Option<Arc<StateLayer>>> {
        if parent.digest() == self.genesis_block.digest() {
            // The genesis block stands for the anchor state: the true genesis for a
            // fresh chain, the pre-consensus tip for a migrated one.
            return Some(self.anchor_layer.clone());
        }
        if let Some(layer) = self.pending.get(&parent.digest()) {
            return Some(Some(layer.clone()));
        }
        if let Some(tip) = self.committed.last()
            && tip.digest() == parent.digest()
        {
            return Some(self.committed_layer.clone());
        }
        None
    }

    fn context_seed_of(&self, layer: &Option<Arc<StateLayer>>) -> ContextSeed {
        match layer {
            Some(layer) => layer.child_context_seed.clone(),
            None => ContextSeed {
                block_hashes: self
                    .genesis
                    .context
                    .block_hashes
                    .push(self.genesis.header_hash),
                next_block_number: 1,
            },
        }
    }

    /// Executes `txs` on top of `parent_layer` through the production VM, replay-style
    /// (all transactions known upfront, no timers). Returns the execution output and
    /// the resulting state layer.
    fn execute(
        &self,
        parent_layer: &Option<Arc<StateLayer>>,
        timestamp: u64,
        txs: &[L2Transaction],
    ) -> anyhow::Result<(BlockOutput, Arc<StateLayer>)> {
        let seed = self.context_seed_of(parent_layer);
        let genesis_context = &self.genesis.context;
        let context = BlockContext {
            chain_id: genesis_context.chain_id,
            block_number: seed.next_block_number,
            block_hashes: seed.block_hashes,
            timestamp,
            // Fee constants held flat across the simulated chain: transactions pay
            // exactly the base fee, so fee-market drift is not modeled.
            eip1559_basefee: U256::from(1_000_000_000u64),
            pubdata_price: U256::ZERO,
            native_price: U256::from(1u64),
            coinbase: Address::ZERO,
            gas_limit: 100_000_000,
            pubdata_limit: 100_000_000,
            mix_hash: U256::ZERO,
            execution_version: genesis_context.execution_version,
            blob_fee: U256::ONE,
        };

        let tx_source = TxListSource {
            transactions: txs
                .iter()
                .map(|tx| ZkTransaction::from(tx.clone()).encode())
                .collect::<VecDeque<_>>(),
        };
        let view = self.view_of(parent_layer.clone());
        let output = zksync_os_multivm::run_block(
            context,
            view.clone(),
            view,
            tx_source,
            NoopTxCallback,
            &mut NopTracer,
            &mut NopValidator,
        )
        .map_err(|err| anyhow::anyhow!("vm run failed: {err:?}"))?;

        for (index, result) in output.tx_results.iter().enumerate() {
            match result {
                Ok(tx_output) if tx_output.is_success() => {}
                Ok(tx_output) => {
                    anyhow::bail!("tx {index} reverted: {:?}", tx_output.execution_result)
                }
                Err(err) => anyhow::bail!("tx {index} invalid: {err:?}"),
            }
        }

        let layer = Arc::new(StateLayer {
            parent: parent_layer.clone(),
            storage: output
                .storage_writes
                .iter()
                .map(|write| (write.key, write.value))
                .collect(),
            preimages: output.published_preimages.iter().cloned().collect(),
            child_context_seed: ContextSeed {
                block_hashes: seed.block_hashes.push(output.header.hash()),
                next_block_number: seed.next_block_number + 1,
            },
        });
        Ok((output, layer))
    }
}

impl ExecutionEnv for RealStfExecution {
    type Block = StfBlock;

    async fn genesis_block(&mut self) -> StfBlock {
        self.inner.lock().unwrap().genesis_block.clone()
    }

    async fn build(&mut self, parent: StfBlock, context: BuildContext) -> Option<StfBlock> {
        let mut inner = self.inner.lock().unwrap();
        let Some(parent_layer) = inner.layer_for_parent(&parent) else {
            warn!(
                height = parent.height_u64() + 1,
                "parent state unavailable; not proposing"
            );
            return None;
        };

        // One transfer per block. The nonce comes from the parent state, so the
        // transaction is valid on exactly this branch; the timestamp is derived from
        // the consensus view, so proposals in different views are distinct blocks.
        let sender_nonce = inner
            .view_of(parent_layer.clone())
            .nonce(inner.sender.address())
            .unwrap_or(0);
        let timestamp = self.genesis_timestamp_plus(&inner, context.view);
        let transfer = make_transfer(
            &inner.sender,
            inner.genesis.context.chain_id,
            sender_nonce,
            parent.height_u64() + 1,
        );

        let (output, layer) =
            match inner.execute(&parent_layer, timestamp, std::slice::from_ref(&transfer)) {
                Ok(result) => result,
                Err(err) => {
                    warn!(?err, "block building failed; not proposing");
                    return None;
                }
            };
        let block = StfBlock::assemble(
            parent.height_u64() + 1,
            parent.era_anchor(),
            parent.digest(),
            timestamp,
            vec![transfer],
            output.header.hash(),
            hash_block_output(&output),
        );
        inner.pending.insert(block.digest(), layer);
        Some(block)
    }

    async fn verify(&mut self, parent: StfBlock, block: StfBlock) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(parent_layer) = inner.layer_for_parent(&parent) else {
            // The parent's state is not at hand (e.g. this validator restarted and lost
            // its pending layers). Withholding the vote is safe; if the network
            // finalizes the block anyway, the commit path re-executes it from the
            // committed chain.
            warn!(
                height = block.height_u64(),
                "parent state unavailable; withholding vote"
            );
            return false;
        };

        // Re-execute the proposer's transactions and require the identical outcome.
        // This is the verify-before-vote guarantee: no honest validator votes for a
        // block whose declared execution result does not reproduce.
        let (output, layer) = match inner.execute(&parent_layer, block.timestamp(), block.txs()) {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    ?err,
                    height = block.height_u64(),
                    "block failed re-execution; rejecting"
                );
                return false;
            }
        };
        let valid = output.header.hash() == block.header_hash()
            && hash_block_output(&output) == block.block_output_hash();
        if valid {
            inner.pending.insert(block.digest(), layer);
        } else {
            warn!(
                height = block.height_u64(),
                "block declared a different execution outcome than re-execution produced; rejecting"
            );
        }
        valid
    }

    async fn committed_height(&mut self) -> Option<Height> {
        // Consensus counts heights from the era anchor; the ledger counts the chain.
        let inner = self.inner.lock().unwrap();
        let tip = inner.committed.len() as u64;
        (inner.anchor_height + tip > 0).then(|| Height::new(tip))
    }

    async fn commit(&mut self, block: StfBlock) {
        let mut inner = self.inner.lock().unwrap();
        let height = block.height_u64();
        assert!(
            height > inner.anchor_height,
            "consensus committed height {height} at or below the anchor {}",
            inner.anchor_height,
        );
        let next_height = inner.anchor_height + inner.committed.len() as u64 + 1;
        if height < next_height {
            // At-least-once delivery after restarts: the same block may arrive again.
            let existing = inner.committed[(height - inner.anchor_height - 1) as usize].digest();
            assert_eq!(
                existing,
                block.digest(),
                "re-committed block at height {height} differs from the committed one"
            );
            return;
        }
        assert_eq!(
            height, next_height,
            "commit out of order: got height {height}, expected {next_height}",
        );

        // Adopt the state layer computed during build/verify, or re-execute if it is
        // not at hand (backfilled block, or a restart lost the pending layers).
        let layer = match inner.pending.get(&block.digest()) {
            Some(layer) => layer.clone(),
            None => {
                let committed_layer = inner.committed_layer.clone();
                let (output, layer) = inner
                    .execute(&committed_layer, block.timestamp(), block.txs())
                    .expect("finalized block must execute");
                assert_eq!(
                    hash_block_output(&output),
                    block.block_output_hash(),
                    "finalized block at height {height} does not reproduce its declared outcome",
                );
                layer
            }
        };
        inner.committed.push(block);
        inner.committed_layer = Some(layer);

        // Layers of abandoned candidates at or below this height can never be parents
        // again; drop them so memory tracks the pending frontier, not history.
        let committed_height = next_height;
        inner
            .pending
            .retain(|_, layer| layer.child_context_seed.next_block_number > committed_height + 1);
    }
}

impl SimEnv for RealStfExecution {
    fn era_anchor(&self) -> u64 {
        self.inner.lock().unwrap().anchor_height
    }

    fn committed_tip(&self) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        let tip = inner.anchor_height + inner.committed.len() as u64;
        (tip > 0).then_some(tip)
    }

    fn committed_chain_digests(&self) -> Vec<Digest> {
        let inner = self.inner.lock().unwrap();
        inner.committed.iter().map(|block| block.digest()).collect()
    }
}

impl RealStfExecution {
    /// Timestamps grow with the consensus view: strictly monotonic along any chain
    /// (views only increase) and unique per proposal attempt.
    fn genesis_timestamp_plus(&self, inner: &Inner, view: u64) -> u64 {
        inner.genesis.context.timestamp + 1 + view
    }
}

/// Builds a signed transfer from the funded test sender. Public so that other test
/// suites can produce executable transactions against the shared genesis.
pub fn make_test_transfer(chain_id: u64, nonce: u64, value: U256) -> L2Transaction {
    let sender = PrivateKeySigner::from_str(SENDER_KEY).expect("valid dev key");
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(TEST_RECIPIENT),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = sender
        .sign_hash_sync(&tx.signature_hash())
        .expect("signing cannot fail");
    let envelope: L2Envelope = tx.into_signed(signature).into();
    Recovered::new_unchecked(envelope, sender.address())
}

fn make_transfer(
    sender: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    height: u64,
) -> L2Transaction {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(TEST_RECIPIENT),
        // The amount encodes the height, so each block's effect on the recipient's
        // balance is predictable and assertable: after committing height H, the
        // recipient holds 1 + 2 + ... + H wei.
        value: U256::from(height),
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = sender
        .sign_hash_sync(&tx.signature_hash())
        .expect("signing cannot fail");
    let envelope: L2Envelope = tx.into_signed(signature).into();
    Recovered::new_unchecked(envelope, sender.address())
}
