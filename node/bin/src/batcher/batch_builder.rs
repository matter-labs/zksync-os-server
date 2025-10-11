use alloy::primitives::Address;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, BatchMetadata, ProverInput};
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_storage_api::ReplayRecord;

/// Takes a vector of blocks and produces a batch envelope.
/// This is a pure function that is meant to be stateless and not contained in the `Batcher` struct.
pub(crate) fn seal_batch(
    blocks: &[(
        BlockOutput,
        ReplayRecord,
        zksync_os_merkle_tree::TreeBatchOutput,
        ProverInput,
    )],
    prev_batch_info: StoredBatchInfo,
    batch_number: u64,
    chain_id: u64,
    chain_address: Address,
) -> anyhow::Result<BatchEnvelope<ProverInput>> {
    let block_number_from = blocks.first().unwrap().1.block_context.block_number;
    let block_number_to = blocks.last().unwrap().1.block_context.block_number;
    let execution_version = blocks.first().unwrap().1.block_context.execution_version;

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
        chain_address,
        batch_number,
    );

    // batch prover input is a concatenation of all blocks' prover inputs with the prepended block count
    let batch_prover_input: ProverInput =
        std::iter::once(u32::try_from(blocks.len()).expect("too many blocks"))
            .chain(
                blocks
                    .iter()
                    .flat_map(|(_, _, _, prover_input)| prover_input.iter().copied()),
            )
            .collect();

    let batch_envelope: BatchEnvelope<ProverInput> = BatchEnvelope::new(
        BatchMetadata {
            previous_stored_batch_info: prev_batch_info,
            batch_info,
            first_block_number: block_number_from,
            last_block_number: block_number_to,
            tx_count: blocks
                .iter()
                .map(|(block_output, _, _, _)| block_output.tx_results.len())
                .sum(),
            execution_version,
        },
        batch_prover_input,
    )
    .with_stage(BatchExecutionStage::BatchSealed);

    Ok(batch_envelope)
}
