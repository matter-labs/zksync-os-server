use crate::verifier::metrics::BATCH_VERIFICATION_RESPONDER_METRICS;
use crate::verify_batch_wire::{VerificationRequest, normalized_commit_data};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use block_cache::BlockCache;
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};
use zksync_os_batch_types::{BatchSignature, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_native_pig::generate_batch_run;
use zksync_os_network::{
    PeerVerifyBatch, PeerVerifyBatchResult, VerifyBatch, VerifyBatchOutcome, VerifyBatchResult,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadFinality, ReadStateHistory};
use zksync_os_storage_api::{StateError, TreeBlock, read_multichain_root};
use zksync_os_types::ProvingVersion;

mod block_cache;
mod metrics;

type VerificationInput = TreeBlock;

/// Batch verification responder that consumes requests from the network.
pub struct BatchVerificationResponder<Finality, ReadState> {
    chain_id: u64,
    diamond_proxy_sl: Address,
    l1_state: L1State,
    signer: PrivateKeySigner,
    block_cache: BlockCache<Finality, TreeBlock>,
    read_state: ReadState,
    merkle_tree: MerkleTree<RocksDBWrapper>,
    verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Batch data mismatch")]
    BatchDataMismatch,
    #[error("State error: {0}")]
    State(#[from] StateError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl<Finality: ReadFinality, ReadState: ReadStateHistory>
    BatchVerificationResponder<Finality, ReadState>
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_id: u64,
        diamond_proxy_sl: Address,
        private_key: SecretString,
        finality: Finality,
        l1_state: L1State,
        read_state: ReadState,
        merkle_tree: MerkleTree<RocksDBWrapper>,
        verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
        outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
    ) -> Self {
        let signer = PrivateKeySigner::from_str(private_key.expose_secret())
            .expect("Invalid batch verification private key");
        if let BatchVerificationSL::Enabled(l1_config) = l1_state.batch_verification.clone()
            && !l1_config.validators.contains(&signer.address())
        {
            tracing::warn!(
                address = %signer.address(),
                "Your address is not authorized to verify batches on L1",
            );
        }

        Self {
            chain_id,
            diamond_proxy_sl,
            l1_state,
            signer,
            block_cache: BlockCache::new(finality),
            read_state,
            merkle_tree,
            verify_request_rx,
            outgoing_verify_results,
        }
    }

    async fn handle_verification_request(
        &self,
        request: VerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            batch_number = request.batch_number,
            request_id = request.request_id,
            "Handling batch verification request (blocks {}-{})",
            request.first_block_number,
            request.last_block_number,
        );

        let blocks = (request.first_block_number..=request.last_block_number)
            .map(|block_number| {
                let cached = self
                    .block_cache
                    .get(block_number)
                    .ok_or(BatchVerificationError::MissingBlock(block_number))?;
                let (block_output, replay_record, tree_data) =
                    (&cached.output, &cached.record, &cached.tree);
                let tree_output = tree_data.output;
                Ok((block_output, replay_record, tree_output))
            })
            .collect::<Result<Vec<_>, BatchVerificationError>>()?;

        let state_view = self.read_state.state_view_at(request.last_block_number)?;
        let multichain_root = read_multichain_root(state_view);
        let (_, last_replay_record, _) = blocks.last().unwrap();
        let protocol_version = blocks.first().unwrap().1.protocol_version.clone();
        let proving_version =
            ProvingVersion::try_from(protocol_version.clone()).map_err(anyhow::Error::from)?;

        let (batch_info, _) = if proving_version == ProvingVersion::V8 {
            let native_batch_run = generate_batch_run(
                proving_version,
                &blocks
                    .iter()
                    .map(|(_, replay_record, _)| (*replay_record).clone())
                    .collect::<Vec<_>>(),
                &self.read_state,
                self.merkle_tree.clone(),
                request.pubdata_mode,
            )
            .map_err(anyhow::Error::from)?;
            ExtendedCommitBatchInfo::build_from_canonical_output(
                request.batch_number,
                request.pubdata_mode,
                &protocol_version,
                native_batch_run
                    .canonical_commit_data(request.first_block_number, request.last_block_number),
            )
            .map_err(anyhow::Error::from)?
        } else {
            ExtendedCommitBatchInfo::build(
                blocks
                    .iter()
                    .map(|(block_output, replay_record, tree)| {
                        (*block_output, replay_record.transactions.as_slice(), tree)
                    })
                    .collect(),
                self.chain_id,
                request.batch_number,
                request.pubdata_mode,
                self.l1_state.sl_chain_id,
                multichain_root,
                &protocol_version,
                &last_replay_record.block_context.block_hashes.0,
            )
        };

        let expected_commit_data = normalized_commit_data(
            batch_info.commit_info.clone(),
            request.execution_protocol_version,
        );
        if expected_commit_data != request.commit_data {
            return Err(BatchVerificationError::BatchDataMismatch);
        }

        let signature = BatchSignature::sign_batch(
            &request.prev_commit_data,
            &batch_info.commit_info,
            self.diamond_proxy_sl,
            self.l1_state.sl_chain_id,
            self.l1_state.validator_timelock_sl,
            &blocks.first().unwrap().1.protocol_version,
            &self.signer,
        )
        .await;

        Ok(signature)
    }

    async fn handle_verification_message(
        &self,
        request: VerifyBatch,
    ) -> Result<VerifyBatchResult, anyhow::Error> {
        let request_id = request.request_id;
        let batch_number = request.batch_number;
        let request = VerificationRequest::try_from(request)?;
        let result = match self.handle_verification_request(request).await {
            Ok(signature) => {
                BATCH_VERIFICATION_RESPONDER_METRICS
                    .record_request_success(request_id, batch_number);
                VerifyBatchOutcome::Approved(signature.into_raw().to_vec().into())
            }
            Err(reason) => {
                BATCH_VERIFICATION_RESPONDER_METRICS
                    .record_request_failure(request_id, batch_number);
                VerifyBatchOutcome::Refused(reason.to_string())
            }
        };
        Ok(VerifyBatchResult {
            request_id,
            batch_number,
            result,
        })
    }
}

#[async_trait]
impl<Finality: ReadFinality, ReadState: ReadStateHistory> PipelineComponent
    for BatchVerificationResponder<Finality, ReadState>
{
    type Input = VerificationInput;
    type Output = ();

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BatchVerificationResponder;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        tracing::info!("starting batch verification responder");
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some(tree_block) => {
                            state_reporter.enter_state(GenericComponentState::Active);
                            let block_number = tree_block.record.block_context.block_number;
                            let block_timestamp = tree_block.record.block_context.timestamp;
                            self.block_cache.insert(block_number, tree_block)?;
                            state_reporter.record_processed(block_number, Some(block_timestamp), None);
                        }
                        None => return Ok(()),
                    }
                }
                request = self.verify_request_rx.recv() => {
                    let Some(request) = request else {
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let peer_id = request.peer_id;
                    let request_id = request.message.request_id;
                    let batch_number = request.message.batch_number;
                    let result = self.handle_verification_message(request.message).await?;
                    tracing::info!(%peer_id, request_id, batch_number, "handled batch verification request");
                    let _ = self.outgoing_verify_results.send(PeerVerifyBatchResult {
                        peer_id,
                        message: result,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::DummyFinality;
    use crate::verify_batch_wire::encode_verify_batch_request;
    use alloy::consensus::{EMPTY_OMMER_ROOT_HASH, Header, Sealable};
    use alloy::eips::eip1559::INITIAL_BASE_FEE;
    use alloy::network::EthereumWallet;
    use alloy::primitives::{Address, B64, B256, Bloom, U256, address, keccak256};
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::mock::Asserter;
    use blake2::{Blake2s256, Digest};
    use std::borrow::Cow;
    use std::collections::{BTreeMap, HashMap};
    use std::ops::RangeInclusive;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Arc;
    use zk_os_api::helpers::{set_properties_code, set_properties_nonce};
    use zk_os_basic_system::system_implementation::flat_storage_model::{
        ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties,
    };
    use zksync_os_batch_types::BlockMerkleTreeData;
    use zksync_os_batch_types::ExtendedCommitBatchInfo;
    use zksync_os_batch_types::batcher_model::{BatchEnvelope, BatchMetadata, ProverInput};
    use zksync_os_contract_interface::models::{BatchDaInputMode, StoredBatchInfo};
    use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
    use zksync_os_contract_interface::{Bridgehub, ZkChain};
    use zksync_os_genesis::{GenesisInput, GenesisState};
    use zksync_os_interface::traits::{PreimageSource, ReadStorage};
    use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper, TreeBatchOutput, TreeEntry};
    use zksync_os_merkle_tree_api::BatchTreeProof;
    use zksync_os_provider::NodeProvider;
    use zksync_os_storage_api::{
        BlockContext, BlockHashes, ReplayRecord, StateError, read_multichain_root,
    };
    use zksync_os_types::{
        BlockOutput, BlockPubdata, BlockStartCursors, ExecutionVersion, ProtocolSemanticVersion,
        PubdataMode,
    };

    const CHAIN_ID: u64 = 270;
    const SL_CHAIN_ID: u64 = 9;
    const BATCH_NUMBER: u64 = 1;
    const REQUEST_ID: u64 = 4242;
    const PRIVATE_KEY: &str = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";
    const DIAMOND_PROXY_SL: Address = address!("0x00000000000000000000000000000000000000d1");
    const VALIDATOR_TIMELOCK: Address = address!("0x00000000000000000000000000000000000000e1");

    #[derive(Clone, Debug)]
    struct MemoryStateView {
        storage: Arc<HashMap<B256, B256>>,
        preimages: Arc<HashMap<B256, Vec<u8>>>,
    }

    impl ReadStorage for MemoryStateView {
        fn read(&mut self, key: B256) -> Option<B256> {
            self.storage.get(&key).copied()
        }
    }

    impl PreimageSource for MemoryStateView {
        fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
            self.preimages.get(&hash).cloned()
        }
    }

    #[derive(Clone, Debug)]
    struct MemoryStateHistory {
        view: MemoryStateView,
        block_range: RangeInclusive<u64>,
    }

    impl MemoryStateHistory {
        fn from_genesis_state(genesis_state: &GenesisState) -> Self {
            let storage = genesis_state
                .storage_logs
                .iter()
                .copied()
                .collect::<HashMap<_, _>>();
            let preimages = genesis_state
                .preimages
                .iter()
                .cloned()
                .collect::<HashMap<_, _>>();

            Self {
                view: MemoryStateView {
                    storage: Arc::new(storage),
                    preimages: Arc::new(preimages),
                },
                block_range: 0..=1,
            }
        }
    }

    impl ReadStateHistory for MemoryStateHistory {
        fn state_view_at(
            &self,
            block_number: u64,
        ) -> Result<impl zksync_os_storage_api::ViewState, StateError> {
            if self.block_range.contains(&block_number) {
                Ok(self.view.clone())
            } else {
                Err(StateError::NotFound(block_number))
            }
        }

        fn block_range_available(&self) -> RangeInclusive<u64> {
            self.block_range.clone()
        }
    }

    #[tokio::test]
    async fn v8_verifier_approves_batch_built_from_native_run() {
        let protocol_version = ProtocolSemanticVersion::new(0, 32, 1);
        let genesis_state = build_genesis_state_for_test(&protocol_version);
        let read_state = MemoryStateHistory::from_genesis_state(&genesis_state);

        let temp_dir = tempfile::tempdir().unwrap();
        let tree = genesis_tree(&genesis_state, temp_dir.path());
        let prev_batch_info = genesis_stored_batch_info(&genesis_state, &tree);
        let tree_block = empty_tree_block(&tree, protocol_version.clone());

        let batch_envelope = v8_batch_for_signing(
            &tree_block,
            prev_batch_info,
            &read_state,
            &tree,
            protocol_version.clone(),
        );
        let request = encode_verify_batch_request(&batch_envelope, REQUEST_ID).unwrap();

        let (_verify_request_tx, verify_request_rx) = mpsc::channel(1);
        let (outgoing_verify_results, _) = broadcast::channel(1);
        let mut responder = BatchVerificationResponder::new(
            CHAIN_ID,
            DIAMOND_PROXY_SL,
            SecretString::from(PRIVATE_KEY.to_owned()),
            DummyFinality::zero(),
            test_l1_state().await,
            read_state.clone(),
            tree.clone(),
            verify_request_rx,
            outgoing_verify_results,
        );
        responder.block_cache.insert(1, tree_block).unwrap();

        let result = responder
            .handle_verification_message(request)
            .await
            .unwrap();

        assert_eq!(result.request_id, REQUEST_ID);
        assert_eq!(result.batch_number, BATCH_NUMBER);

        let signature = match result.result {
            VerifyBatchOutcome::Approved(signature) => {
                let signature: [u8; 65] = signature.as_ref().try_into().unwrap();
                BatchSignature::from_raw_array(&signature).unwrap()
            }
            VerifyBatchOutcome::Refused(reason) => panic!("verification refused: {reason}"),
        };

        let validated = signature
            .verify_signature(
                &batch_envelope.batch.previous_stored_batch_info,
                &batch_envelope.batch.batch_info.commit_info,
                DIAMOND_PROXY_SL,
                SL_CHAIN_ID,
                VALIDATOR_TIMELOCK,
                &protocol_version,
            )
            .unwrap();
        let expected_signer = PrivateKeySigner::from_str(PRIVATE_KEY).unwrap().address();
        assert_eq!(*validated.signer(), expected_signer);
    }

    fn v8_batch_for_signing<ReadState: ReadStateHistory>(
        tree_block: &TreeBlock,
        prev_batch_info: StoredBatchInfo,
        read_state: &ReadState,
        tree: &MerkleTree<RocksDBWrapper>,
        protocol_version: ProtocolSemanticVersion,
    ) -> zksync_os_batch_types::batcher_model::BatchForSigning<ProverInput> {
        let native_batch_run = generate_batch_run(
            ProvingVersion::V8,
            &[tree_block.record.clone()],
            read_state,
            tree.clone(),
            PubdataMode::Calldata,
        )
        .unwrap();
        let (batch_info, blob_sidecar) = ExtendedCommitBatchInfo::build_from_canonical_output(
            BATCH_NUMBER,
            PubdataMode::Calldata,
            &protocol_version,
            native_batch_run.canonical_commit_data(1, 1),
        )
        .unwrap();

        let multichain_root = read_multichain_root(read_state.state_view_at(1).unwrap());

        BatchEnvelope::new(
            BatchMetadata {
                previous_stored_batch_info: prev_batch_info,
                batch_info,
                chain_address: DIAMOND_PROXY_SL,
                blob_sidecar,
                first_block_number: 1,
                last_block_number: 1,
                last_block_hash: Some(tree_block.output.header.hash()),
                pubdata_mode: PubdataMode::Calldata,
                tx_count: tree_block.output.tx_results.len(),
                computational_native_used: Some(tree_block.output.computational_native_used),
                logs: vec![],
                messages: vec![],
                multichain_root,
                set_sl_chain_id_migration_number: None,
            },
            ProverInput::Real(native_batch_run.prover_input),
        )
    }

    fn empty_tree_block(
        tree: &MerkleTree<RocksDBWrapper>,
        protocol_version: ProtocolSemanticVersion,
    ) -> TreeBlock {
        let (root_hash, leaf_count) = tree.root_info(0).unwrap().unwrap();
        let tree_output = TreeBatchOutput {
            root_hash,
            leaf_count,
        };

        TreeBlock {
            output: empty_block_output(),
            record: empty_replay_record(protocol_version),
            tree: BlockMerkleTreeData {
                input: tree_output,
                output: TreeBatchOutput {
                    root_hash,
                    leaf_count,
                },
                written_keys: vec![],
                read_keys: vec![],
                proof: BatchTreeProof {
                    operations: vec![],
                    read_operations: vec![],
                    sorted_leaves: BTreeMap::new(),
                    hashes: vec![],
                },
            },
        }
    }

    fn empty_block_output() -> BlockOutput {
        let header = Header {
            number: 1,
            timestamp: 1,
            ..Default::default()
        }
        .seal_slow();

        BlockOutput {
            header,
            tx_results: vec![],
            storage_writes: vec![],
            account_diffs: vec![],
            published_preimages: vec![],
            pubdata: BlockPubdata::Length(0),
            computational_native_used: 0,
        }
    }

    fn empty_replay_record(protocol_version: ProtocolSemanticVersion) -> ReplayRecord {
        ReplayRecord::new(
            BlockContext {
                chain_id: CHAIN_ID,
                block_number: 1,
                block_hashes: BlockHashes::default(),
                timestamp: 1,
                eip1559_basefee: U256::from(INITIAL_BASE_FEE),
                pubdata_price: U256::ZERO,
                native_price: U256::ONE,
                coinbase: Address::ZERO,
                gas_limit: 100_000_000,
                pubdata_limit: 100_000_000,
                mix_hash: U256::ZERO,
                execution_version: ExecutionVersion::V7 as u32,
                blob_fee: U256::ONE,
            },
            vec![],
            0,
            semver::Version::new(0, 0, 0),
            protocol_version,
            B256::ZERO,
            vec![],
            BlockStartCursors::default(),
        )
    }

    async fn test_l1_state() -> L1State {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .wallet(EthereumWallet::default())
            .connect_mocked_client(asserter.clone());
        let provider = NodeProvider::new(provider);

        asserter.push_failure(ErrorPayload {
            code: -32601,
            message: Cow::Borrowed("method missing"),
            data: None,
        });

        let diamond_proxy_l1 = ZkChain::new(
            address!("0x00000000000000000000000000000000000000c1"),
            provider.clone(),
        );
        let settlement_layer_intervals = SettlementLayerIntervals::discover(
            address!("0x00000000000000000000000000000000000000b1"),
            diamond_proxy_l1.clone(),
            None,
            CHAIN_ID,
        )
        .await
        .unwrap();

        L1State {
            bridgehub_l1: Bridgehub::new(
                address!("0x00000000000000000000000000000000000000a1"),
                provider.clone(),
                CHAIN_ID,
            ),
            bridgehub_sl: Bridgehub::new(
                address!("0x00000000000000000000000000000000000000a2"),
                provider.clone(),
                CHAIN_ID,
            ),
            diamond_proxy_l1,
            diamond_proxy_sl: ZkChain::new(DIAMOND_PROXY_SL, provider),
            validator_timelock_sl: VALIDATOR_TIMELOCK,
            batch_verification: BatchVerificationSL::Disabled,
            last_committed_batch: 0,
            last_proved_batch: 0,
            last_executed_batch: 0,
            last_finalized_executed_batch: 0,
            sl_block_number: 0,
            finalized_sl_block_number: 0,
            da_input_mode: BatchDaInputMode::Rollup,
            l1_chain_id: 1,
            sl_chain_id: SL_CHAIN_ID,
            settlement_layer_address: Address::ZERO,
            settlement_layer_intervals,
        }
    }

    fn build_genesis_state_for_test(protocol_version: &ProtocolSemanticVersion) -> GenesisState {
        let genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../local-chains/v31.0/default/genesis.json");
        let genesis_input = GenesisInput::load_from_file(&genesis_path).unwrap();

        let mut storage_logs: BTreeMap<B256, B256> = BTreeMap::new();
        let mut preimages = vec![];

        for (address, deployed_code) in genesis_input.initial_contracts {
            let mut account_properties = AccountProperties::default();
            set_properties_nonce(&mut account_properties, 1);
            let bytecode_preimage = set_properties_code(&mut account_properties, &deployed_code);
            let bytecode_hash = account_properties.bytecode_hash;

            let flat_storage_key = account_properties_flat_key(address);
            let account_properties_hash = account_properties.compute_hash();
            storage_logs.insert(
                flat_storage_key,
                account_properties_hash.as_u8_array().into(),
            );

            preimages.push((bytecode_hash.as_u8_array().into(), bytecode_preimage));
            preimages.push((
                account_properties_hash.as_u8_array().into(),
                account_properties.encoding().to_vec(),
            ));
        }

        for (key, value) in genesis_input.additional_storage_raw {
            let duplicate = storage_logs.insert(key, value).is_some();
            assert!(
                !duplicate,
                "duplicate key in additional_storage_raw: {key:?}"
            );
        }
        for (key, value, address_and_key) in genesis_input.additional_storage.into_storage_slots() {
            let duplicate = storage_logs.insert(key, value).is_some();
            assert!(
                !duplicate,
                "duplicate flattened key in additional_storage: {address_and_key:?}"
            );
        }
        for (hash, preimage) in genesis_input.additional_preimages {
            preimages.push((hash, alloy::hex::decode(preimage).unwrap()));
        }

        let header = Header {
            parent_hash: B256::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            difficulty: U256::ZERO,
            number: 0,
            gas_limit: 5_000,
            gas_used: 0,
            timestamp: 0,
            extra_data: Default::default(),
            mix_hash: B256::ZERO,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(INITIAL_BASE_FEE),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
            block_access_list_hash: None,
            slot_number: None,
        }
        .seal_slow();

        GenesisState {
            storage_logs: storage_logs.into_iter().collect(),
            preimages,
            header,
            context: BlockContext {
                chain_id: CHAIN_ID,
                block_number: 0,
                block_hashes: Default::default(),
                timestamp: 0,
                eip1559_basefee: U256::from(INITIAL_BASE_FEE),
                pubdata_price: U256::ZERO,
                native_price: U256::ONE,
                coinbase: Address::ZERO,
                gas_limit: 100_000_000,
                pubdata_limit: 100_000_000,
                mix_hash: U256::ZERO,
                execution_version: ExecutionVersion::try_from(protocol_version).unwrap() as u32,
                blob_fee: U256::ONE,
            },
            expected_genesis_root: genesis_input.genesis_root,
        }
    }

    fn genesis_tree(
        genesis_state: &GenesisState,
        path: &std::path::Path,
    ) -> MerkleTree<RocksDBWrapper> {
        let db = RocksDBWrapper::new(path).unwrap();
        let mut tree = MerkleTree::new(db).unwrap();
        let tree_entries = genesis_state
            .storage_logs
            .iter()
            .map(|(key, value)| TreeEntry {
                key: *key,
                value: *value,
            })
            .collect::<Vec<_>>();
        tree.extend(&tree_entries).unwrap();
        tree
    }

    fn genesis_stored_batch_info(
        genesis_state: &GenesisState,
        tree: &MerkleTree<RocksDBWrapper>,
    ) -> StoredBatchInfo {
        let (genesis_root_hash, genesis_root_leaves) = tree.root_info(0).unwrap().unwrap();

        let last_256_block_hashes_blake = {
            let mut blocks_hasher = Blake2s256::new();
            for _ in 0..255 {
                blocks_hasher.update([0u8; 32]);
            }
            blocks_hasher.update(genesis_state.header.hash());
            blocks_hasher.finalize()
        };

        let mut hasher = Blake2s256::new();
        hasher.update(genesis_root_hash.as_slice());
        hasher.update(genesis_root_leaves.to_be_bytes());
        hasher.update(0u64.to_be_bytes());
        hasher.update(last_256_block_hashes_blake);
        hasher.update(0u64.to_be_bytes());
        let state_commitment = B256::from_slice(&hasher.finalize());

        assert_eq!(genesis_state.expected_genesis_root, state_commitment);

        StoredBatchInfo {
            batch_number: 0,
            state_commitment,
            number_of_layer1_txs: 0,
            priority_operations_hash: keccak256([]),
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            commitment: B256::from(U256::ONE.to_be_bytes()),
            last_block_timestamp: Some(0),
        }
    }

    fn account_properties_flat_key(address: Address) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[12..32].copy_from_slice(address.as_slice());
        flat_storage_key_for_contract(
            ACCOUNT_PROPERTIES_STORAGE_ADDRESS.to_be_bytes().into(),
            bytes.into(),
        )
    }

    fn flat_storage_key_for_contract(address: Address, key: B256) -> B256 {
        let mut bytes = [0u8; 64];
        bytes[12..32].copy_from_slice(address.as_slice());
        bytes[32..64].copy_from_slice(key.as_slice());
        B256::from_slice(Blake2s256::digest(bytes).as_slice())
    }
}
