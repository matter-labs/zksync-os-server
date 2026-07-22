//! The consensus block builder against the real VM and a detached mempool: the
//! leader-side twin of the `env` tests' verify/commit coverage.
//!
//! The pool is a [`Pool::new_detached`] — real subpools, no L1 watchers — so
//! tests seed deposits and upgrades directly and the builder consumes them
//! through the same streams production uses. The L2 side is the production
//! reth pool over an in-memory state view; tests leave it empty (L2 traffic is
//! the sequencer tests' subject — the builder's own logic is numbering, L1
//! inclusion, upgrade gating, and commit-time pool bookkeeping).

use alloy::primitives::{Address, B256, U256};
use std::collections::HashMap;
use std::sync::Arc;
use zksync_os_consensus_core::idle_policy::IdlePolicy;
use zksync_os_consensus_execution::builder::{BuilderConfig, ConsensusBlockBuilder, ParentInfo};
use zksync_os_consensus_sim::stf::{SharedGenesis, shared_genesis, test_sender_address};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_mempool::Pool;
use zksync_os_mempool::subpools::l1::L1Subpool;
use zksync_os_mempool::subpools::upgrade::UpgradeSubpool;
use zksync_os_sequencer::execution::{FeeParams, FeeProvider};
use zksync_os_storage_api::{
    BlockHashes, ReadStateHistory, RepositoryBlock, RepositoryResult, StateResult, ViewState,
};
use zksync_os_types::{
    BlockStartCursors, L1Envelope, L1PriorityEnvelope, L1Tx, ProtocolSemanticVersion, UpgradeInfo,
    UpgradeMetadata,
};

const FUNDING: u128 = 10u128.pow(24);
const CHAIN_ID: u64 = 270;

/// Genesis-only state: the builder executes empty/deposit/upgrade blocks, none
/// of which need post-genesis history.
#[derive(Clone)]
struct GenesisState {
    genesis: Arc<SharedGenesis>,
}

#[derive(Clone)]
struct GenesisView {
    storage: Arc<HashMap<B256, B256>>,
    preimages: Arc<HashMap<B256, Vec<u8>>>,
}

impl ReadStorage for GenesisView {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.storage.get(&key).copied()
    }
}

impl PreimageSource for GenesisView {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.preimages.get(&hash).cloned()
    }
}

impl std::fmt::Debug for GenesisState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GenesisState")
    }
}

impl GenesisState {
    fn view(&self) -> GenesisView {
        GenesisView {
            storage: Arc::new(self.genesis.storage.clone()),
            preimages: Arc::new(self.genesis.preimages.clone()),
        }
    }
}

impl ReadStateHistory for GenesisState {
    fn state_view_at(&self, _block_number: u64) -> StateResult<impl ViewState> {
        Ok(self.view())
    }

    fn block_range_available(&self) -> std::ops::RangeInclusive<u64> {
        0..=u64::MAX
    }
}

/// Repository stub: the provider factory reads only the genesis block header.
#[derive(Clone, Debug)]
struct GenesisRepository {
    genesis_hash: B256,
}

impl zksync_os_storage_api::LogIndex for GenesisRepository {}

impl zksync_os_storage_api::ReadRepository for GenesisRepository {
    fn get_block_by_number(&self, number: u64) -> RepositoryResult<Option<RepositoryBlock>> {
        if number != 0 {
            return Ok(None);
        }
        let header = alloy::consensus::Header {
            number: 0,
            gas_limit: 100_000_000,
            base_fee_per_gas: Some(1_000),
            ..Default::default()
        };
        Ok(Some(RepositoryBlock::new_unchecked(
            alloy::consensus::Block {
                header,
                body: Default::default(),
            },
            self.genesis_hash,
        )))
    }

    fn get_block_by_hash(&self, _hash: B256) -> RepositoryResult<Option<RepositoryBlock>> {
        Ok(None)
    }

    fn get_raw_transaction(
        &self,
        _hash: alloy::primitives::TxHash,
    ) -> RepositoryResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn get_transaction(
        &self,
        _hash: alloy::primitives::TxHash,
    ) -> RepositoryResult<Option<zksync_os_types::ZkTransaction>> {
        Ok(None)
    }

    fn get_transaction_receipt(
        &self,
        _hash: alloy::primitives::TxHash,
    ) -> RepositoryResult<Option<zksync_os_types::ZkReceiptEnvelope>> {
        Ok(None)
    }

    fn get_transaction_meta(
        &self,
        _hash: alloy::primitives::TxHash,
    ) -> RepositoryResult<Option<zksync_os_storage_api::TxMeta>> {
        Ok(None)
    }

    fn get_transaction_hash_by_sender_nonce(
        &self,
        _sender: Address,
        _nonce: u64,
    ) -> RepositoryResult<Option<alloy::primitives::TxHash>> {
        Ok(None)
    }

    fn get_stored_transaction(
        &self,
        _hash: alloy::primitives::TxHash,
    ) -> RepositoryResult<Option<zksync_os_storage_api::StoredTxData>> {
        Ok(None)
    }

    fn get_latest_block(&self) -> u64 {
        0
    }
}

/// The pool's `Genesis` handle: constructed against a dead endpoint — a
/// detached pool initialized past the genesis block never consults it.
async fn offline_genesis() -> zksync_os_genesis::Genesis {
    #[derive(Debug)]
    struct NeverLoaded;
    #[async_trait::async_trait]
    impl zksync_os_genesis::GenesisInputSource for NeverLoaded {
        async fn genesis_input(&self) -> anyhow::Result<zksync_os_genesis::GenesisInput> {
            panic!("a detached pool initialized past genesis never builds genesis state");
        }
    }

    let signer = alloy::signers::local::PrivateKeySigner::random();
    let provider = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .connect_http("http://127.0.0.1:9".parse().expect("static url"));
    let provider = zksync_os_provider::NodeProvider::new(provider)
        .await
        .expect("offline construction probes and degrades, never fails");
    let zk_chain = zksync_os_contract_interface::ZkChain::new(Address::repeat_byte(0xEC), provider);
    zksync_os_genesis::Genesis::new(Arc::new(NeverLoaded), zk_chain, CHAIN_ID)
}

fn test_runtime() -> reth_tasks::Runtime {
    reth_tasks::RuntimeBuilder::new(reth_tasks::RuntimeConfig::default().with_tokio(
        reth_tasks::TokioConfig::existing_handle(tokio::runtime::Handle::current()),
    ))
    .build()
    .expect("failed to build runtime")
}

fn version(semver: &str) -> ProtocolSemanticVersion {
    semver.parse().expect("valid version")
}

fn deposit(serial: u64) -> Arc<L1PriorityEnvelope> {
    Arc::new(L1Envelope {
        inner: L1Tx {
            hash: B256::repeat_byte(0x07),
            initiator: Address::repeat_byte(1),
            to: Address::repeat_byte(2),
            gas_limit: 500_000,
            gas_per_pubdata_byte_limit: 800,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            nonce: serial,
            value: U256::from(1_000_000u64),
            to_mint: U256::from(1_000_000u64),
            refund_recipient: Address::repeat_byte(1),
            input: alloy::primitives::Bytes::new(),
            factory_deps: Vec::new(),
            marker: std::marker::PhantomData,
        },
    })
}

fn upgrade_info(target: &str) -> UpgradeInfo {
    let target = version(target);
    UpgradeInfo {
        tx: Some(L1Envelope {
            inner: L1Tx {
                hash: B256::repeat_byte(0xAA),
                initiator: Address::repeat_byte(3),
                to: Address::repeat_byte(4),
                gas_limit: 72_000_000,
                gas_per_pubdata_byte_limit: 800,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: target.minor,
                value: U256::ZERO,
                to_mint: U256::ZERO,
                refund_recipient: Address::repeat_byte(3),
                input: alloy::primitives::Bytes::new(),
                factory_deps: Vec::new(),
                marker: std::marker::PhantomData,
            },
        }),
        metadata: UpgradeMetadata {
            timestamp: 0,
            protocol_version: target,
            force_preimages: Vec::new(),
        },
    }
}

/// The whole leader-side rig: a detached pool over the shared test genesis and
/// the production builder on top of it, initialized at a parent past genesis
/// (block 2) the way a consensus leader resumes mid-chain.
struct Rig<S: zksync_os_mempool::subpools::l2::L2Subpool> {
    builder: ConsensusBlockBuilder<S>,
    genesis: Arc<SharedGenesis>,
    l1: L1Subpool,
    upgrades: UpgradeSubpool,
    l2: S,
    /// The fee provider polls these; dropping them reads as a closed channel.
    _fee_feeds: (
        tokio::sync::watch::Sender<Option<U256>>,
        tokio::sync::watch::Sender<Option<num::rational::Ratio<u64>>>,
    ),
}

async fn rig() -> Rig<impl zksync_os_mempool::subpools::l2::L2Subpool> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .try_init();
    {
        let genesis = shared_genesis(&[(test_sender_address(), U256::from(FUNDING))]);
        let state = GenesisState {
            genesis: genesis.clone(),
        };
        let repository = GenesisRepository {
            genesis_hash: genesis.header_hash,
        };
        let l2 = zksync_os_mempool::subpools::l2::in_memory(
            zksync_os_reth_compat::provider::ZkProviderFactory::new(state, repository, CHAIN_ID),
            zksync_os_mempool::PoolConfig::default(),
            zksync_os_mempool::TxValidatorConfig {
                max_input_bytes: 128 * 1024,
            },
        );

        let l2_handle = l2.clone();
        let l1 = L1Subpool::new(16);
        let upgrades = UpgradeSubpool::default();
        let mut pool = Pool::new_detached(
            test_runtime(),
            offline_genesis().await,
            16,
            upgrades.clone(),
            l1.clone(),
            l2,
        );

        let parent_record = parent_record();
        pool.init(
            &parent_record,
            zksync_os_consensus_execution::builder::derive_next_cursors(&parent_record),
        )
        .await;

        let fee_config = zksync_os_sequencer::execution::FeeConfig {
            native_price_usd: num::rational::Ratio::from_integer(1u32.into()),
            base_fee_override: None,
            native_per_gas: 1,
            pubdata_price_override: None,
            pubdata_price_cap: None,
            native_price_override: None,
        };
        let (pubdata_tx, pubdata_rx) = tokio::sync::watch::channel(Some(U256::from(100)));
        let (blob_tx, blob_rx) = tokio::sync::watch::channel(None);
        let fee_provider = FeeProvider::new(
            fee_config,
            pubdata_rx,
            blob_rx,
            zksync_os_base_token_adjuster::BaseTokenPriceHandle::published(
                zksync_os_types::TokenPricesForFees {
                    base_token_usd_price:
                        zksync_os_types::TokenApiRatio::from_f64_decimals_and_timestamp(
                            1.0, 0, None,
                        ),
                    sl_token_usd_price:
                        zksync_os_types::TokenApiRatio::from_f64_decimals_and_timestamp(
                            1.0, 0, None,
                        ),
                },
            ),
            Some(zksync_os_types::PubdataMode::Calldata),
        );

        let config = BuilderConfig {
            l2_chain_id: CHAIN_ID,
            sl_chain_id: 900,
            gas_limit: 100_000_000,
            pubdata_limit: 110_000,
            fee_collector_address: Address::repeat_byte(9),
            block_time: std::time::Duration::from_millis(150),
            idle_block_deadline: std::time::Duration::from_millis(50),
            max_transactions_in_block: 100,
            interop_roots_per_block: 16,
            era_anchor: 0,
        };

        let builder = ConsensusBlockBuilder::new(pool, fee_provider, config, IdlePolicy::legacy());
        Rig {
            builder,
            genesis,
            l1,
            upgrades,
            l2: l2_handle,
            _fee_feeds: (pubdata_tx, blob_tx),
        }
    }
}

impl<S: zksync_os_mempool::subpools::l2::L2Subpool> Rig<S> {
    fn parent(&self) -> ParentInfo {
        use commonware_cryptography::sha256::Digest;
        ParentInfo {
            number: 2,
            timestamp: self.genesis.context.timestamp + 2,
            el_hash: self.genesis.header_hash,
            block_hashes: BlockHashes::default(),
            protocol_version: version("0.31.0"),
            next_cursors: BlockStartCursors::default(),
            carries_upgrade_tx: false,
            fee_params: FeeParams {
                eip1559_basefee: self.genesis.context.eip1559_basefee,
                native_price: self.genesis.context.native_price,
                pubdata_price: self.genesis.context.pubdata_price,
            },
            digest: Digest::from([0u8; 32]),
        }
    }

    fn view(&self) -> GenesisView {
        GenesisView {
            storage: Arc::new(self.genesis.storage.clone()),
            preimages: Arc::new(self.genesis.preimages.clone()),
        }
    }
}

/// A structurally-valid record for block 2: the pool's init anchor (never
/// replayed — consensus fast-forwards to its WAL tip without replaying).
fn parent_record() -> zksync_os_storage_api::ReplayRecord {
    zksync_os_storage_api::ReplayRecord {
        block_context: zksync_os_storage_api::BlockContext {
            chain_id: CHAIN_ID,
            block_number: 2,
            block_hashes: Default::default(),
            timestamp: 1_700_000_002,
            execution_version: 31,
            gas_limit: 100_000_000,
            pubdata_limit: 110_000,
            coinbase: Address::repeat_byte(9),
            eip1559_basefee: U256::from(1_000),
            native_price: U256::from(1_000),
            pubdata_price: U256::from(100),
            blob_fee: U256::ONE,
            mix_hash: U256::ZERO,
        },
        transactions: Vec::new(),
        previous_block_timestamp: 1_700_000_001,
        node_version: semver::Version::new(0, 0, 0),
        protocol_version: version("0.31.0"),
        block_output_hash: B256::ZERO,
        force_preimages: Vec::new(),
        starting_cursors: BlockStartCursors::default(),
    }
}

/// An idle leader turn under the legacy policy: an empty block, numbered
/// exactly one past the parent, consuming nothing from the L1 cursors.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_build_numbers_the_child_and_leaves_cursors_alone() {
    let mut rig = rig().await;
    let parent = rig.parent();
    let built = rig
        .builder
        .build(&parent, rig.view())
        .await
        .expect("legacy idle policy always builds");
    assert_eq!(built.record.block_context.block_number, 3);
    assert!(built.record.transactions.is_empty());
    assert_eq!(built.next_cursors, parent.next_cursors);
    assert_eq!(built.record.previous_block_timestamp, parent.timestamp);

    // The consensus stack enters through the `BuildBlocks` trait; same answer.
    use zksync_os_consensus_execution::builder::BuildBlocks;
    let view = rig.view();
    let built = BuildBlocks::<GenesisView>::build_block(&mut rig.builder, &parent, view)
        .await
        .expect("the trait entry point builds too");
    assert_eq!(built.record.block_context.block_number, 3);
}

/// A pooled L2 transaction flows through the production stream into the block
/// and executes. (Deposits and upgrades need L1-derived fixtures the unit tier
/// does not have; their inclusion paths are covered by the consensus
/// integration tests, which run real deposits through real nodes.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pooled_l2_transaction_is_included() {
    use zksync_os_mempool::subpools::l2::L2Subpool as _;
    let mut rig = rig().await;
    rig.l2
        .add_l2_transaction(zksync_os_consensus_sim::stf::make_test_transfer(
            CHAIN_ID,
            0,
            U256::from(12_345u64),
        ))
        .await
        .expect("the transfer is valid for the pool");

    let built = rig
        .builder
        .build(&rig.parent(), rig.view())
        .await
        .expect("builds with the transfer");
    assert_eq!(built.record.transactions.len(), 1);
    assert!(matches!(
        built.record.transactions[0].envelope(),
        zksync_os_types::ZkEnvelope::L2(_)
    ));
    assert_eq!(
        built.next_cursors.l1_priority_id, 0,
        "an L2 transfer consumes no L1 cursor"
    );
}

/// Upgrade gating follows the parent's carry state: a watched same-version
/// upgrade the parent does *not* carry rides first (the genesis-continuation
/// shape the validity rules accept), and the moment the parent carries it, the
/// pool's still-open offer is stale and must not ride again. (The
/// version-bumping half of the gate needs an upgrade transaction the VM
/// accepts for the *next* version — an L1-derived fixture the unit tier does
/// not have; wire-shaped dummies are dropped at execution.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_gating_follows_the_parents_carry_state() {
    let mut rig = rig().await;
    rig.upgrades.insert(upgrade_info("0.31.0")).await;
    let built = rig
        .builder
        .build(&rig.parent(), rig.view())
        .await
        .expect("builds");
    assert!(
        matches!(
            built.record.transactions.first().map(|tx| tx.envelope()),
            Some(zksync_os_types::ZkEnvelope::Upgrade(_))
        ),
        "an uncarried watched upgrade rides as the first transaction"
    );
    assert_eq!(built.record.protocol_version, version("0.31.0"));

    // The parent now carries it, but the pool has not seen that commit yet —
    // an offer/carry inconsistency the builder refuses to build over: the
    // leader passes its turn instead of re-proposing the upgrade.
    let mut parent = rig.parent();
    parent.carries_upgrade_tx = true;
    assert!(
        rig.builder.build(&parent, rig.view()).await.is_none(),
        "a still-offered, already-carried upgrade fails the turn"
    );
}

/// Commit-time bookkeeping: `on_committed` is what moves the pool's canonical
/// L1 watermark — a finalized block carrying a deposit marks it *seen*. The
/// committed block is hand-written (commit bookkeeping trusts finalized
/// content; it does not re-execute).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_committed_marks_the_deposit_seen() {
    use zksync_os_consensus_execution::builder::BuildBlocks;
    let mut rig = rig().await;
    rig.l1.clone().insert(deposit(0)).await;

    let inputs = rig.builder.pool().l1_inputs_view();
    let (pending, watermark_before) = inputs.seen_priority_tx(0).await;
    assert!(pending.is_some(), "the watched deposit starts out seen");

    let mut committed = parent_record();
    committed.block_context.block_number = 3;
    committed.transactions = vec![zksync_os_types::ZkTransaction::from((*deposit(0)).clone())];
    let header = alloy::consensus::Header {
        number: 3,
        ..Default::default()
    };
    BuildBlocks::<GenesisView>::on_committed(
        &mut rig.builder,
        alloy::consensus::Sealed::new_unchecked(header, B256::repeat_byte(0x33)),
        &[],
        &committed,
    )
    .await;
    let (seen, watermark_after) = inputs.seen_priority_tx(0).await;
    assert!(
        seen.is_none(),
        "a deposit passed by the committed chain leaves the seen set"
    );
    assert_eq!(
        watermark_after, watermark_before,
        "draining is not seeing: the seen watermark stays"
    );
}
