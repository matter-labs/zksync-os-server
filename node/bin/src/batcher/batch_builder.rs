use alloy::primitives::Address;
use zksync_os_batch_types::BatchInfo;
use zksync_os_contract_interface::models::{L2Log, StoredBatchInfo};
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    BatchEnvelope, BatchForSigning, BatchMetadata, ProverInput,
};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, read_multichain_root};
use zksync_os_types::{ProvingVersion, PubdataMode};

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
) -> anyhow::Result<BatchForSigning<ProverInput>> {
    let block_number_from = blocks.first().unwrap().1.block_context.block_number;
    let block_number_to = blocks.last().unwrap().1.block_context.block_number;
    let execution_version = blocks.first().unwrap().1.block_context.execution_version;
    let protocol_version = blocks.first().unwrap().1.protocol_version.clone();

    let state_view = read_state.state_view_at(block_number_to)?;
    let multichain_root = read_multichain_root(state_view);
    let batch_info = BatchInfo::new(
        blocks
            .iter()
            .map(|(block_output, replay_record, tree, _)| {
                (
                    block_output,
                    &replay_record.block_context,
                    replay_record.transactions.as_slice(),
                    tree,
                )
            })
            .collect(),
        chain_id,
        chain_address_sl,
        batch_number,
        pubdata_mode,
        sl_chain_id,
        multichain_root,
        &protocol_version,
    );

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

    use zk_os_forward_system::run::generate_batch_proof_input;
    use zk_os_forward_system_dev::run::generate_batch_proof_input as generate_batch_proof_input_dev;

    let proving_version =
        ProvingVersion::try_from(blocks.first().unwrap().1.protocol_version.clone())?;
    // execution version should be the same for all the blocks, it is ensured by the seal criteria
    let batch_prover_input: ProverInput = match proving_version {
        ProvingVersion::V1
        | ProvingVersion::V2
        | ProvingVersion::V3
        | ProvingVersion::V4
        | ProvingVersion::V5 => {
            panic!("sealing batch with prover version v1-v5 is not supported");
        }
        ProvingVersion::V6 => {
            // TODO: in the long-term we should generate proof input per batch
            generate_batch_proof_input(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.as_slice())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.pubdata.as_slice())
                    .collect(),
            )
        }
        ProvingVersion::V7 => {
            // TODO: in the long-term we should generate proof input per batch
            generate_batch_proof_input_dev(
                blocks
                    .iter()
                    .map(|(_, _, _, prover_input)| prover_input.as_slice())
                    .collect(),
                (pubdata_mode.da_commitment_scheme() as u8)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?,
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.pubdata.as_slice())
                    .collect(),
            )
        }
    };

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

    let batch_envelope = BatchEnvelope::new(
        BatchMetadata {
            previous_stored_batch_info: prev_batch_info,
            batch_info,
            first_block_number: block_number_from,
            last_block_number: block_number_to,
            pubdata_mode,
            tx_count: blocks
                .iter()
                .map(|(block_output, _, _, _)| block_output.tx_results.len())
                .sum(),
            execution_version,
            protocol_version,
            computational_native_used: Some(
                blocks
                    .iter()
                    .map(|(block_output, _, _, _)| block_output.computational_native_used)
                    .sum(),
            ),
            logs,
            messages,
            multichain_root,
        },
        batch_prover_input,
    )
    .with_stage(BatchExecutionStage::BatchSealed);

    Ok(batch_envelope)
}
