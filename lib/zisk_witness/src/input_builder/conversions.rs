//! Converting server-side blocks, transactions and logs into guest input form.

use super::*;

/// Extract the inner calldata bytes from an ABI-encoded
/// `L2CanonicalTransaction` (field 14 is the `data` offset, relative to the
/// outer struct offset of 32).
///
/// Returns `None` on any out-of-bounds offset/length or an offset/length that
/// does not fit in `usize`, so a malformed or unexpectedly-shaped encoding
/// degrades the ZiSK build gracefully instead of panicking the shared pipeline
/// task on an unchecked slice.
pub(super) fn abi_l2_canonical_calldata(abi_data: &[u8]) -> Option<&[u8]> {
    let data_rel: usize = U256::from_be_slice(abi_data.get(32 + 14 * 32..32 + 15 * 32)?)
        .try_into()
        .ok()?;
    let data_abs = 32usize.checked_add(data_rel)?;
    let data_len: usize = U256::from_be_slice(abi_data.get(data_abs..data_abs.checked_add(32)?)?)
        .try_into()
        .ok()?;
    let start = data_abs.checked_add(32)?;
    abi_data.get(start..start.checked_add(data_len)?)
}

pub(super) fn extract_block_hashes(
    hashes: &zksync_os_storage_api::BlockHashes,
    block_number: u64,
) -> Vec<(u64, B256)> {
    hashes
        .0
        .iter()
        .enumerate()
        .filter_map(|(i, hash)| {
            let h = B256::from(hash.to_be_bytes::<32>());
            if !h.is_zero() && block_number > 0 {
                // Map index to block number: hashes[0] = current-256, hashes[255] = current-1
                // For block 1: hashes[255] = genesis block (block 0)
                let offset = 256u64.saturating_sub(i as u64);
                if offset <= block_number {
                    let num = block_number - offset;
                    Some((num, h))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn extract_l2_to_l1_logs(block_output: &BlockOutput) -> Vec<L2ToL1LogEntry> {
    let mut logs = Vec::new();
    for tx_result in block_output.tx_results.iter().flatten() {
        for log in &tx_result.l2_to_l1_logs {
            logs.push(L2ToL1LogEntry {
                l2_shard_id: log.log.l2_shard_id,
                is_service: log.log.is_service,
                tx_number_in_block: log.log.tx_number_in_block,
                sender: log.log.sender,
                key: log.log.key,
                value: log.log.value,
            });
        }
    }
    logs
}

// ---------------------------------------------------------------------------
// Transaction conversion
// ---------------------------------------------------------------------------

pub(super) fn convert_all_txs(
    transactions: &[ZkTransaction],
    block_output: &BlockOutput,
) -> Vec<TxInput> {
    transactions
        .iter()
        .enumerate()
        .map(|(i, tx)| {
            let mut tx_input = convert_tx(tx);
            // Include the server's gas_used for all transactions.
            // REVM's gas computation may differ from ZKsync OS native gas
            // (especially for L1 deposits and upgrade txs), so the server's
            // gas value is authoritative for block header computation.
            match block_output.tx_results.get(i) {
                Some(Ok(result)) => {
                    tx_input.gas_used_override = Some(result.gas_used);
                }
                Some(Err(_)) => {
                    tx_input.gas_used_override = Some(0);
                    tx_input.force_fail = true;
                }
                None => {}
            }
            tx_input
        })
        .collect()
}

pub(super) fn convert_tx(tx: &ZkTransaction) -> TxInput {
    use alloy::sol_types::SolValue;

    // Helper to ABI-encode an L1/upgrade tx as L2CanonicalTransaction.
    fn abi_encode_l1<T: zksync_os_types::L1TxType>(
        i: &zksync_os_types::L1Tx<T>,
        tx_type_byte: u8,
    ) -> Vec<u8> {
        zksync_os_contract_interface::L2CanonicalTransaction {
            txType: U256::from(tx_type_byte),
            from: U256::from_be_slice(i.initiator.as_slice()),
            to: U256::from_be_slice(i.to.as_slice()),
            gasLimit: U256::from(i.gas_limit),
            gasPerPubdataByteLimit: U256::from(i.gas_per_pubdata_byte_limit),
            maxFeePerGas: U256::from(i.max_fee_per_gas),
            maxPriorityFeePerGas: U256::from(i.max_priority_fee_per_gas),
            paymaster: U256::ZERO,
            nonce: U256::from(i.nonce),
            value: U256::from(i.value),
            reserved: [
                U256::from(i.to_mint),
                U256::from_be_slice(i.refund_recipient.as_slice()),
                U256::ZERO,
                U256::ZERO,
            ],
            data: i.input().to_vec().into(),
            signature: Default::default(),
            factoryDeps: i
                .factory_deps
                .iter()
                .map(|h| U256::from_be_bytes(h.0))
                .collect(),
            paymasterInput: Default::default(),
            reservedDynamic: Default::default(),
        }
        .abi_encode()
    }

    let auth = match tx.envelope() {
        ZkEnvelope::System(system_envelope) => TxAuth::System {
            tx_hash: *system_envelope.hash(),
            encoded_2718: system_envelope.encoded_2718(),
        },
        ZkEnvelope::L2(_) => TxAuth::L2 {
            signed_bytes: tx.envelope().encoded_2718(),
        },
        ZkEnvelope::L1(l1) => {
            let i = &l1.inner;
            TxAuth::L1 {
                tx_hash: i.hash,
                abi_encoded: abi_encode_l1(i, 0x7f),
            }
        }
        ZkEnvelope::Upgrade(u) => {
            let i = &u.inner;
            TxAuth::Upgrade {
                tx_hash: i.hash,
                abi_encoded: abi_encode_l1(i, 0x7e),
            }
        }
    };

    TxInput {
        chain_id: tx.envelope().chain_id(),
        gas_used_override: None,
        force_fail: false,
        auth,
    }
}

// ---------------------------------------------------------------------------
// Tracking database for pre-execution
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_storage_api::BlockHashes;

    fn hashes_with(entries: &[(usize, u8)]) -> BlockHashes {
        let mut arr = [U256::ZERO; 256];
        for &(i, byte) in entries {
            arr[i] = U256::from_be_bytes(B256::repeat_byte(byte).0);
        }
        BlockHashes(arr)
    }

    /// The ring is oldest-first: index 255 (offset 1) is the parent block.
    /// For block 1 the parent is genesis (block 0); index 254 (offset 2) would
    /// be block -1 and is out of range, so it is dropped.
    #[test]
    fn genesis_maps_index_255_to_block_zero() {
        let hashes = hashes_with(&[(255, 0xaa), (254, 0xbb)]);
        assert_eq!(
            extract_block_hashes(&hashes, 1),
            vec![(0u64, B256::repeat_byte(0xaa))]
        );
    }

    /// offset = 256 - i, num = block_number - offset: index 0 is the oldest
    /// (current-256), index 255 is the newest (current-1).
    #[test]
    fn index_zero_is_oldest_index_255_is_newest() {
        let hashes = hashes_with(&[(0, 0x11), (255, 0x22)]);
        let mut out = extract_block_hashes(&hashes, 300);
        out.sort_by_key(|(n, _)| *n);
        assert_eq!(
            out,
            vec![
                (44u64, B256::repeat_byte(0x11)),
                (299u64, B256::repeat_byte(0x22)),
            ]
        );
    }

    /// An entry whose offset exceeds the current block number maps to a
    /// negative block and is dropped.
    #[test]
    fn out_of_range_offsets_are_dropped() {
        // block 200: index 0 (offset 256) > 200 -> dropped;
        //            index 100 (offset 156) -> block 44.
        let hashes = hashes_with(&[(0, 0x11), (100, 0x33)]);
        assert_eq!(
            extract_block_hashes(&hashes, 200),
            vec![(44u64, B256::repeat_byte(0x33))]
        );
    }

    #[test]
    fn zero_hashes_are_filtered() {
        assert!(extract_block_hashes(&hashes_with(&[]), 300).is_empty());
    }

    #[test]
    fn block_zero_yields_nothing() {
        assert!(extract_block_hashes(&hashes_with(&[(255, 0xaa)]), 0).is_empty());
    }
}
