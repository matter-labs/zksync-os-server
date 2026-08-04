use alloy::primitives::Address;
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_batch_types::batcher_model::{
    BatchEnvelope, BatchForSigning, BatchMetadata, ProverInput,
};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::models::{L2Log, StoredBatchInfo};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, read_multichain_root};
use zksync_os_types::{BlockOutput, ProvingVersion, PubdataMode, SystemTxType, ZkEnvelope};

use crate::batcher::zisk_batch::SecondProofSystemConfig;
use crate::zisk_bytes::{ZiskBatchBytes, ZiskBlockBytes};
use zisk_prover_lane::shadow_execute_zisk_batch;

/// Takes a vector of blocks and produces a batch envelope.
///
/// Returns the envelope plus the assembled per-batch second proof-system
/// bytes. The bytes are `Some` only when the second proof system is enabled
/// and assembly succeeds; the caller routes them out-of-band (opening a ZiSK
/// job on the job manager), keeping the shared `ProverInput` free of ZiSK data.
///
/// The second proof-system inputs travel only on the enabled path.
/// `second_proof` is `Some` only when the feature is on. `zisk_blocks` carries
/// the per-block bytes in block order; it is `None` when the feature is off or
/// when a block's ZiSK input degraded. The batch-boundary tree views are built
/// inside `zisk_witness::assemble_batch` from the tree handle on `second_proof`.
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
    chain_address: Address,
    pubdata_mode: PubdataMode,
    sl_chain_id: u64,
    read_state: &ReadState,
    second_proof: Option<&SecondProofSystemConfig>,
    zisk_blocks: Option<Vec<ZiskBlockBytes>>,
) -> anyhow::Result<(BatchForSigning<ProverInput>, Option<ZiskBatchBytes>)> {
    let block_number_from = blocks.first().unwrap().1.block_context.block_number;
    let block_number_to = blocks.last().unwrap().1.block_context.block_number;
    let last_block_hash = blocks.last().unwrap().0.header.hash();
    let protocol_version = blocks.first().unwrap().1.protocol_version.clone();
    let (_, last_replay_record, _, _) = blocks.last().unwrap();

    let state_view = read_state.state_view_at(block_number_to)?;
    let multichain_root = read_multichain_root(state_view);
    let (batch_info, blob_sidecar) = PendingBatchInfo::build(
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

    let proving_version =
        ProvingVersion::try_from(blocks.first().unwrap().1.protocol_version.clone())?;
    // execution version should be the same for all the blocks, it is ensured by the seal criteria
    // Airbender batch witness (primary proof system).
    let batch_prover_input = compute_batch_prover_input(blocks, proving_version, pubdata_mode)?;

    // Assemble the second proof-system input only when it is enabled and every
    // per-block input is present. This call is fail-open. An assembly error
    // degrades this batch's ZiSK data to `None` and is logged. A failure in
    // the secondary lane must never stop the primary Airbender batch from
    // sealing. The per-block guard keeps the same contract (see
    // `guard_zisk_build`). A Fake batch carries no witness, so there is
    // nothing to assemble. The batch-boundary tree views, after-state
    // preimages and referenced bytecodes are all built inside
    // `zisk_witness::assemble_batch`.
    let zisk_batch_data: Option<ZiskBatchBytes> = match (second_proof, zisk_blocks) {
        (Some(second_proof), Some(zisk_blocks))
            if !matches!(batch_prover_input, ProverInput::Fake) =>
        {
            let assembled = zisk_witness::assemble_batch(
                blocks,
                &zisk_blocks,
                read_state,
                &second_proof.merkle_tree,
                pubdata_mode,
                multichain_root,
                sl_chain_id,
                &batch_info,
                &blob_sidecar,
                second_proof.chain_config,
            );
            match assembled {
                Ok(data) => Some(ZiskBatchBytes(data)),
                Err(error) => {
                    tracing::warn!(
                        "ZiSK batch assembly failed: {error:#}; degrading this batch's ZiSK data \
                         to None (primary Airbender lane unaffected)"
                    );
                    None
                }
            }
        }
        _ => None,
    };

    if let (Some(second_proof), Some(zisk_data)) = (second_proof, &zisk_batch_data)
        && let Some(shadow) = second_proof.shadow
    {
        shadow_execute_zisk_batch(
            zisk_data.as_slice(),
            &prev_batch_info.state_commitment,
            &batch_info,
            chain_id,
            second_proof.chain_config,
            shadow.halt_on_mismatch,
            blocks,
        )?;
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
            chain_address,
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

    Ok((batch_envelope, zisk_batch_data))
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
) -> anyhow::Result<ProverInput> {
    use zk_os_forward_system::run::generate_batch_proof_input;
    use zk_os_forward_system_prev::run::generate_batch_proof_input as generate_batch_proof_input_prev;

    if blocks
        .iter()
        .any(|(_, _, _, pi)| matches!(pi, ProverInput::Fake))
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
    })
}
