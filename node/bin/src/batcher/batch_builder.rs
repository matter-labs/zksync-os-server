use alloy::primitives::Address;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    BatchEnvelope, BatchForSigning, BatchMetadata, ProverInput,
};
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{ExecutionVersion, ProvingVersion, PubdataMode};

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
    pubdata_mode: PubdataMode,
) -> anyhow::Result<BatchForSigning<ProverInput>> {
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
        pubdata_mode,
    );

    use zk_os_forward_system::run::generate_batch_proof_input;

    let forward_run_execution_version =
        ExecutionVersion::try_from(blocks.first().unwrap().1.block_context.execution_version)
            .expect("Must be valid execution as set by the server");
    // execution version should be the same for all the blocks, it is ensured by the seal criteria
    let batch_prover_input: ProverInput =
        match ProvingVersion::from_forward_run_execution_version(forward_run_execution_version) {
            ProvingVersion::V1 | ProvingVersion::V2 | ProvingVersion::V3 => {
                panic!("sealing batch with prover version v1-v3 is not supported");
            }
            ProvingVersion::V4 => {
                std::iter::once(u32::try_from(blocks.len()).expect("too many blocks"))
                    .chain(
                        blocks
                            .iter()
                            .flat_map(|(_, _, _, prover_input)| prover_input.iter().copied()),
                    )
                    .collect()
            }
            ProvingVersion::V5 => {
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
        };

    let protocol_version = blocks.first().unwrap().1.protocol_version.clone();
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
        },
        batch_prover_input,
    )
    .with_stage(BatchExecutionStage::BatchSealed);

    Ok(batch_envelope)
}
