use alloy::primitives::Address;
use zksync_os_batch_types::batcher_model::{
    BatchEnvelope, BatchForSigning, BatchMetadata, ProverInput,
};
use zksync_os_batch_types::{CanonicalBatchCommitData, ExtendedCommitBatchInfo};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::models::{L2Log, StoredBatchInfo};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_native_pig::{NativeBatchRunOutput, generate_batch_run};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, read_multichain_root};
use zksync_os_types::{BlockOutput, ProvingVersion, PubdataMode, SystemTxType, ZkEnvelope};

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

    let state_view = read_state.state_view_at(block_number_to)?;
    let multichain_root = read_multichain_root(state_view);
    let native_batch_run = match proving_version {
        ProvingVersion::V8 => Some(generate_batch_run(
            proving_version,
            &blocks
                .iter()
                .map(|(_, replay_record, _, _)| replay_record.clone())
                .collect::<Vec<_>>(),
            read_state,
            merkle_tree.clone(),
            pubdata_mode,
        )?),
        _ => None,
    };
    let (batch_info, blob_sidecar) = if let Some(native_batch_run) = &native_batch_run {
        ExtendedCommitBatchInfo::build_from_canonical_output(
            batch_number,
            pubdata_mode,
            &protocol_version,
            CanonicalBatchCommitData {
                first_block_number: block_number_from,
                last_block_number: block_number_to,
                first_block_timestamp: native_batch_run.first_block_timestamp,
                last_block_timestamp: native_batch_run.last_block_timestamp,
                new_state_commitment: native_batch_run.new_state_commitment,
                da_commitment: native_batch_run.da_commitment,
                number_of_layer1_txs: native_batch_run.number_of_layer1_txs,
                number_of_layer2_txs: native_batch_run.number_of_layer2_txs,
                priority_operations_hash: native_batch_run.priority_operations_hash,
                dependency_roots_rolling_hash: native_batch_run.dependency_roots_rolling_hash,
                l2_to_l1_logs_root_hash: native_batch_run.l2_to_l1_logs_root_hash,
                upgrade_tx_hash: native_batch_run.upgrade_tx_hash,
                chain_id: native_batch_run.chain_id,
                sl_chain_id: native_batch_run.sl_chain_id,
                pubdata: native_batch_run.pubdata.clone(),
            },
        )?
    } else {
        ExtendedCommitBatchInfo::build(
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
    let batch_prover_input =
        compute_batch_prover_input(blocks, proving_version, pubdata_mode, native_batch_run)?;

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
            computational_native_used: Some(
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.computational_native_used)
                    .sum(),
            ),
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
) -> anyhow::Result<ProverInput> {
    use zk_os_forward_system::run::generate_batch_proof_input;
    use zk_os_forward_system_prev::run::generate_batch_proof_input as generate_batch_proof_input_prev;

    if proving_version != ProvingVersion::V8
        && blocks
            .iter()
            .any(|(_, _, _, prover_input)| matches!(prover_input, ProverInput::Fake))
    {
        return Ok(ProverInput::Fake);
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
            ProverInput::Real(generate_batch_proof_input_prev(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.unwrap_real())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.pubdata.as_slice())
                    .collect(),
            ))
        }
        ProvingVersion::V7 => {
            // TODO: in the long-term we should generate proof input per batch
            ProverInput::Real(generate_batch_proof_input(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.unwrap_real())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.pubdata.as_slice())
                    .collect(),
            ))
        }
        ProvingVersion::V8 => ProverInput::Real(
            native_batch_run
                .expect("V8 prover input must be computed via native batch run")
                .prover_input,
        ),
    })
}
