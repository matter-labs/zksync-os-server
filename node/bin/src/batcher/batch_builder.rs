use crate::pig_telemetry::{BatchPigMode, BatchPigTelemetry, record_batch_pig_telemetry};
use alloy::primitives::Address;
use std::time::Duration;
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_batch_types::batcher_model::{
    BatchEnvelope, BatchForSigning, BatchMetadata, ProverInput,
};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::models::{L2Log, StoredBatchInfo};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_native_pig::{NativeBatchRunOutput, generate_batch_run};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, read_multichain_root};
use zksync_os_types::{BlockOutput, ProvingVersion, PubdataMode, SystemTxType, ZkEnvelope};

#[derive(Debug, Clone, Copy)]
struct BatchPigMeasurement {
    mode: BatchPigMode,
    prover_input_words: usize,
    elapsed: Duration,
}

/// Takes a vector of blocks and produces a batch envelope.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seal_batch<ReadState: ReadStateHistory>(
    blocks: &[(
        BlockOutput,
        ReplayRecord,
        zksync_os_merkle_tree::TreeBatchOutput,
        ProverInput,
    )],
    prev_batch_info: StoredBatchInfo,
    batch_number: u64,
    chain_id: u64,
    chain_address_sl: Address,
    pubdata_mode: PubdataMode,
    sl_chain_id: u64,
    read_state: &ReadState,
    merkle_tree: &MerkleTree<RocksDBWrapper>,
) -> anyhow::Result<BatchForSigning<ProverInput>> {
    let block_number_from = blocks.first().unwrap().1.block_context.block_number;
    let block_number_to = blocks.last().unwrap().1.block_context.block_number;
    let last_block_hash = blocks.last().unwrap().0.header.hash();
    let protocol_version = blocks.first().unwrap().1.protocol_version.clone();
    let (_, last_replay_record, _, _) = blocks.last().unwrap();
    let proving_version = ProvingVersion::try_from(protocol_version.clone())?;
    let batch_computational_native_used: u64 = blocks
        .iter()
        .map(|(block_output, _, _, _)| block_output.computational_native_used)
        .sum();

    let state_view = read_state.state_view_at(block_number_to)?;
    let multichain_root = read_multichain_root(state_view);
    let native_batch_run = match proving_version {
        ProvingVersion::V8 => {
            let started_at = std::time::Instant::now();
            let batch_run = generate_batch_run(
                proving_version,
                &blocks
                    .iter()
                    .map(|(_, replay_record, _, _)| replay_record.clone())
                    .collect::<Vec<_>>(),
                read_state,
                merkle_tree.clone(),
                pubdata_mode,
            )?;
            let elapsed = started_at.elapsed();
            record_batch_pig_telemetry(BatchPigTelemetry {
                batch_number,
                chain_id,
                first_block_number: block_number_from,
                last_block_number: block_number_to,
                proving_version,
                mode: BatchPigMode::NativeBatch,
                prover_input_words: batch_run.prover_input.len(),
                computational_native_used: batch_computational_native_used,
                elapsed,
            });
            Some(batch_run)
        }
        _ => None,
    };
    if let Some(native_batch_run) = &native_batch_run {
        tracing::info!(
            batch_number,
            block_number_from,
            block_number_to,
            block_count = blocks.len(),
            ?protocol_version,
            ?proving_version,
            pubdata_mode = ?pubdata_mode,
            node_sl_chain_id = sl_chain_id,
            native_sl_chain_id = native_batch_run.sl_chain_id,
            native_chain_id = native_batch_run.chain_id,
            prover_input_words = native_batch_run.prover_input.len(),
            canonical_pubdata_bytes = native_batch_run.pubdata.len(),
            "Using native batch PIG for batch sealing",
        );
    }

    let (batch_info, blob_sidecar) = if let Some(native_batch_run) = &native_batch_run {
        PendingBatchInfo::build_from_canonical_output(
            batch_number,
            pubdata_mode,
            &protocol_version,
            native_batch_run.canonical_commit_data(block_number_from, block_number_to),
        )?
    } else {
        PendingBatchInfo::build(
            blocks
                .iter()
                .map(|(block_output, replay_record, tree, _)| {
                    (block_output, replay_record.transactions.as_slice(), tree)
                })
                .collect(),
            chain_id,
            batch_number,
            pubdata_mode,
            sl_chain_id,
            multichain_root,
            &protocol_version,
            &last_replay_record.block_context.block_hashes.0,
        )
    };

    let mut logs = Vec::new();
    let mut messages = Vec::new();
    for block in blocks {
        for output in block.0.tx_results.iter().flatten() {
            for l2_to_l1_log in &output.l2_to_l1_logs {
                logs.push(L2Log {
                    l2_shard_id: l2_to_l1_log.log.l2_shard_id,
                    is_service: l2_to_l1_log.log.is_service,
                    tx_number_in_batch: l2_to_l1_log.log.tx_number_in_block,
                    sender: l2_to_l1_log.log.sender,
                    key: l2_to_l1_log.log.key,
                    value: l2_to_l1_log.log.value,
                });
                if let Some(preimage) = l2_to_l1_log.preimage.as_ref() {
                    messages.push(preimage.clone());
                }
            }
        }
    }

    // execution version should be the same for all the blocks, it is ensured by the seal criteria
    let (batch_prover_input, batch_pig_measurement) =
        compute_batch_prover_input(blocks, proving_version, pubdata_mode, native_batch_run)?;
    if let Some(batch_pig_measurement) = batch_pig_measurement {
        record_batch_pig_telemetry(BatchPigTelemetry {
            batch_number,
            chain_id,
            first_block_number: block_number_from,
            last_block_number: block_number_to,
            proving_version,
            mode: batch_pig_measurement.mode,
            prover_input_words: batch_pig_measurement.prover_input_words,
            computational_native_used: batch_computational_native_used,
            elapsed: batch_pig_measurement.elapsed,
        });
    }

    // Sanity check: all blocks in the batch should have the same protocol version
    for (_, replay_record, _, _) in blocks.iter().skip(1) {
        anyhow::ensure!(
            replay_record.protocol_version == protocol_version,
            "mismatched protocol versions in batch: expected {}, found {}; blocks: {:?}",
            protocol_version,
            replay_record.protocol_version,
            blocks,
        );
    }

    // Detect any `SetSLChainId` system transaction across all blocks in the batch.
    // Excludes the sentinel value `u64::MAX` which is used during protocol upgrades and is
    // unrelated to gateway migrations.
    let set_sl_chain_id_migration_number = blocks.iter().find_map(|(_, replay_record, _, _)| {
        replay_record.transactions.iter().find_map(|tx| {
            if let ZkEnvelope::System(system_tx) = tx.envelope()
                && let SystemTxType::SetSLChainId(_, n) = system_tx.system_subtype()
                && *n != u64::MAX
            {
                Some(*n)
            } else {
                None
            }
        })
    });

    let batch_envelope = BatchEnvelope::new(
        BatchMetadata {
            previous_stored_batch_info: prev_batch_info,
            batch_info,
            chain_address: chain_address_sl,
            blob_sidecar,
            first_block_number: block_number_from,
            last_block_number: block_number_to,
            last_block_hash: Some(last_block_hash),
            pubdata_mode,
            tx_count: blocks
                .iter()
                .map(|(block_output, _, _, _)| block_output.tx_results.len())
                .sum(),
            computational_native_used: Some(batch_computational_native_used),
            logs,
            messages,
            multichain_root,
            set_sl_chain_id_migration_number,
        },
        batch_prover_input,
    )
    .with_stage(BatchExecutionStage::BatchSealed);

    Ok(batch_envelope)
}

fn compute_batch_prover_input(
    blocks: &[(
        BlockOutput,
        ReplayRecord,
        zksync_os_merkle_tree::TreeBatchOutput,
        ProverInput,
    )],
    proving_version: ProvingVersion,
    pubdata_mode: PubdataMode,
    native_batch_run: Option<NativeBatchRunOutput>,
) -> anyhow::Result<(ProverInput, Option<BatchPigMeasurement>)> {
    use zk_os_forward_system::run::generate_batch_proof_input;
    use zk_os_forward_system_prev::run::generate_batch_proof_input as generate_batch_proof_input_prev;

    // Pre-V8 batch PIG stitches together the per-block prover inputs, so a single fake
    // block input makes the real batch input impossible to build - the whole batch
    // falls back to a fake input.
    // V8 intentionally skips this: the native batch run has already been executed from
    // replay records in `seal_batch` (it is required for the canonical batch commit
    // data regardless of proving) and never reads per-block inputs, so its real prover
    // input comes for free even when block inputs are fake.
    if proving_version < ProvingVersion::V8
        && blocks
            .iter()
            .any(|(_, _, _, prover_input)| matches!(prover_input, ProverInput::Fake))
    {
        return Ok((ProverInput::Fake, None));
    }

    Ok(match proving_version {
        ProvingVersion::V1
        | ProvingVersion::V2
        | ProvingVersion::V3
        | ProvingVersion::V4
        | ProvingVersion::V5 => {
            panic!("sealing batch with prover version v1-v5 is not supported");
        }
        ProvingVersion::V6 => {
            // TODO: in the long-term we should generate proof input per batch
            let started_at = std::time::Instant::now();
            let prover_input = generate_batch_proof_input_prev(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.unwrap_real())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.expect_pubdata_bytes())
                    .collect(),
            );
            let prover_input_words = prover_input.len();
            (
                ProverInput::Real(prover_input),
                Some(BatchPigMeasurement {
                    mode: BatchPigMode::LegacyBatch,
                    prover_input_words,
                    elapsed: started_at.elapsed(),
                }),
            )
        }
        ProvingVersion::V7 => {
            // TODO: in the long-term we should generate proof input per batch
            let started_at = std::time::Instant::now();
            let prover_input = generate_batch_proof_input(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.unwrap_real())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.expect_pubdata_bytes())
                    .collect(),
            );
            let prover_input_words = prover_input.len();
            (
                ProverInput::Real(prover_input),
                Some(BatchPigMeasurement {
                    mode: BatchPigMode::LegacyBatch,
                    prover_input_words,
                    elapsed: started_at.elapsed(),
                }),
            )
        }
        ProvingVersion::V8 => (
            ProverInput::Real(
                native_batch_run
                    .expect("V8 prover input must be computed via native batch run")
                    .prover_input,
            ),
            None,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::compute_batch_prover_input;
    use alloy::consensus::{Header, Sealed};
    use alloy::primitives::{Address, B256, U256};
    use semver::Version;
    use zksync_os_batch_types::batcher_model::ProverInput;
    use zksync_os_merkle_tree::TreeBatchOutput;
    use zksync_os_native_pig::NativeBatchRunOutput;
    use zksync_os_storage_api::{BlockContext, BlockHashes, ReplayRecord};
    use zksync_os_types::{
        BlockOutput, BlockPubdata, BlockStartCursors, ExecutionVersion, ProtocolSemanticVersion,
        ProvingVersion, PubdataMode,
    };

    fn dummy_block_output() -> BlockOutput {
        let header = Header {
            number: 1,
            timestamp: 11,
            ..Default::default()
        };
        BlockOutput {
            header: Sealed::new_unchecked(header, B256::ZERO),
            tx_results: vec![],
            storage_writes: vec![],
            account_diffs: vec![],
            published_preimages: vec![],
            pubdata: BlockPubdata::Length(0),
            computational_native_used: 0,
        }
    }

    fn dummy_replay_record() -> ReplayRecord {
        ReplayRecord::new(
            BlockContext {
                chain_id: 270,
                block_number: 1,
                block_hashes: BlockHashes::default(),
                timestamp: 11,
                eip1559_basefee: U256::ZERO,
                pubdata_price: U256::ZERO,
                native_price: U256::ZERO,
                coinbase: Address::ZERO,
                gas_limit: 0,
                pubdata_limit: 0,
                mix_hash: U256::ZERO,
                execution_version: ExecutionVersion::V7 as u32,
                blob_fee: U256::ZERO,
            },
            vec![],
            10,
            Version::new(0, 0, 0),
            ProtocolSemanticVersion::new(0, 32, 0),
            B256::ZERO,
            vec![],
            BlockStartCursors::default(),
        )
    }

    fn dummy_tree_output() -> TreeBatchOutput {
        TreeBatchOutput {
            root_hash: B256::ZERO,
            leaf_count: 2,
        }
    }

    #[test]
    fn v8_batch_prover_input_comes_from_native_batch_run() {
        let (prover_input, batch_pig_measurement) = compute_batch_prover_input(
            &[],
            ProvingVersion::V8,
            PubdataMode::Calldata,
            Some(NativeBatchRunOutput {
                prover_input: vec![7, 8, 9],
                pubdata: vec![],
                previous_state_commitment: B256::ZERO,
                batch_public_input_hash: B256::ZERO,
                new_state_commitment: B256::ZERO,
                da_commitment: B256::ZERO,
                number_of_layer1_txs: 0,
                number_of_layer2_txs: 0,
                priority_operations_hash: B256::ZERO,
                dependency_roots_rolling_hash: B256::ZERO,
                l2_to_l1_logs_root_hash: B256::ZERO,
                first_block_timestamp: 0,
                last_block_timestamp: 0,
                chain_id: 0,
                sl_chain_id: 0,
                upgrade_tx_hash: None,
            }),
        )
        .unwrap();

        assert!(batch_pig_measurement.is_none());
        assert!(matches!(prover_input, ProverInput::Real(ref words) if words == &[7, 8, 9]));
    }

    #[test]
    fn pre_v8_batch_with_fake_block_input_stays_fake() {
        let block = (
            dummy_block_output(),
            dummy_replay_record(),
            dummy_tree_output(),
            ProverInput::Fake,
        );

        let (prover_input, batch_pig_measurement) =
            compute_batch_prover_input(&[block], ProvingVersion::V7, PubdataMode::Calldata, None)
                .unwrap();

        assert!(batch_pig_measurement.is_none());
        assert!(matches!(prover_input, ProverInput::Fake));
    }
}
