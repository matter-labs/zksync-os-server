//! Recovering deployed bytecode from blake2s-keyed preimage blobs.

use super::*;

/// Recover the raw EVM code whose `keccak256` equals `keccak_key` from a
/// blake2s preimage blob (`code || padding || jumpdest-artifacts`) and push it
/// into the guest's keccak-keyed bytecode map.
///
/// `unpadded_code_len` is the fast path: it holds for virtually every account,
/// but it is not reliable for all (observed off for some upgrade
/// force-deploy targets, leaving alignment padding in the slice). Since the
/// observable hash IS `keccak256(unpadded code)`, the code is the unique blob
/// prefix that hashes to it, so on a fast-path miss we scan prefix lengths to
/// recover exactly that code. A wrong-keyed entry would poison the map the
/// guest verifies with `keccak256(code) == key`, so nothing is pushed unless
/// it matches. Returns whether an entry was accepted.
pub(super) fn push_code_from_blob(
    keccak_key: B256,
    blob: &[u8],
    unpadded_code_len: usize,
    bytecodes_map: &mut HashMap<B256, Bytecode>,
    bytecodes_out: &mut Vec<(B256, Vec<u8>)>,
) -> bool {
    let Some(code) = recover_code_matching(keccak_key, blob, unpadded_code_len) else {
        tracing::debug!(
            key = %keccak_key, blob_len = blob.len(),
            "no blob prefix reproduces the keccak key; not preloading"
        );
        return false;
    };
    if let std::collections::hash_map::Entry::Vacant(entry) = bytecodes_map.entry(keccak_key) {
        bytecodes_out.push((keccak_key, code.to_vec()));
        entry.insert(Bytecode::new_raw(Bytes::copy_from_slice(code.as_slice())));
    }
    true
}

/// Blob prefix lengths just below `unpadded_code_len` scanned on a fast-path
/// miss. The recorded length is only ever off by 32-byte alignment padding for
/// some upgrade force-deploy targets, so the true code prefix sits within this
/// window; scanning it (instead of every prefix) keeps the miss path linear.
const ALIGNMENT_WINDOW: usize = 64;

/// Cap on the last-resort prefix scan used only when `unpadded_code_len` gives
/// no usable anchor. A prefix scan hashes 1 + 2 + ... + n bytes, so leaving it
/// unbounded lets a large upgrade-block blob burn seconds of CPU in the SHARED
/// prover-input blocking task; capping it bounds that cost. An
/// un-recovered code only fails THIS batch's ZiSK proof later at worst; it
/// never stalls or crashes input generation.
const MAX_ANCHORLESS_SCAN: usize = 8 * 1024;

/// Recover the raw EVM code whose `keccak256` equals `keccak_key` from a
/// blake2s preimage blob (`code || padding || jumpdest-artifacts`).
/// `unpadded_code_len` is the fast path; on a miss the unique matching blob
/// prefix is found by a BOUNDED scan (see `push_code_from_blob`).
/// `None` if no prefix matches within the bound.
pub fn recover_code_matching(
    keccak_key: B256,
    blob: &[u8],
    unpadded_code_len: usize,
) -> Option<Vec<u8>> {
    let matches =
        |n: usize| n <= blob.len() && alloy::primitives::keccak256(&blob[..n]) == keccak_key;

    // Fast path: the recorded unpadded length. Holds for virtually every account.
    if unpadded_code_len > 0 && matches(unpadded_code_len) {
        return Some(blob[..unpadded_code_len].to_vec());
    }

    if unpadded_code_len > 0 {
        // Recorded length off by alignment padding: the true prefix is just
        // below it. A bounded window instead of the pathological O(n^2) full
        // prefix scan that stalls the shared task on large upgrade blobs.
        let lo = unpadded_code_len.saturating_sub(ALIGNMENT_WINDOW).max(1);
        for n in (lo..unpadded_code_len).rev() {
            if matches(n) {
                return Some(blob[..n].to_vec());
            }
        }
        return None;
    }

    // No usable anchor (recorded length is zero): bounded last-resort scan.
    let scan_upper = blob.len().min(MAX_ANCHORLESS_SCAN);
    (1..=scan_upper)
        .find(|&n| matches(n))
        .map(|n| blob[..n].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `code || padding || artifacts` blob with the raw code padded up to a
    /// 32-byte boundary before the artifacts, mirroring the real preimage
    /// layout.
    fn blob_with(code: &[u8], artifacts_len: usize) -> Vec<u8> {
        let padded = code.len().div_ceil(32) * 32;
        let mut blob = code.to_vec();
        blob.resize(padded + artifacts_len, 0xAB);
        blob
    }

    #[test]
    fn fast_path_recovers_exact_code() {
        let code: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
        let key = alloy::primitives::keccak256(&code);
        let blob = blob_with(&code, 40);
        assert_eq!(
            recover_code_matching(key, &blob, code.len()),
            Some(code.clone())
        );
    }

    /// When the recorded length carries alignment padding (too large),
    /// the bounded window below it still recovers the exact code.
    #[test]
    fn recovers_when_recorded_len_includes_padding() {
        let code: Vec<u8> = (0..100u32).map(|i| (i * 7) as u8).collect();
        let key = alloy::primitives::keccak256(&code);
        let blob = blob_with(&code, 40);
        // Recorded length is the padded length (128), not the true 100.
        let padded_len = code.len().div_ceil(32) * 32;
        assert_eq!(
            recover_code_matching(key, &blob, padded_len),
            Some(code.clone())
        );
    }

    /// A wrong key never yields a (poisoning) entry: the guest verifies
    /// `keccak256(code) == key`, so a non-match must return `None`.
    #[test]
    fn no_match_returns_none() {
        let blob = blob_with(&[1, 2, 3, 4], 8);
        assert_eq!(
            recover_code_matching(B256::repeat_byte(0x99), &blob, 4),
            None
        );
    }

    /// `unpadded_code_len == 0` gives no anchor, so recovery falls back to the
    /// bounded prefix scan and still finds the unique matching prefix.
    #[test]
    fn anchorless_scan_recovers_code() {
        let code: Vec<u8> = (0..80u32).map(|i| (i * 3) as u8).collect();
        let key = alloy::primitives::keccak256(&code);
        let blob = blob_with(&code, 32);
        assert_eq!(recover_code_matching(key, &blob, 0), Some(code.clone()));
    }

    /// An empty blob can match nothing, whether or not a recorded length is
    /// supplied, so recovery returns `None` instead of panicking on the slice.
    #[test]
    fn empty_blob_returns_none() {
        assert_eq!(recover_code_matching(B256::repeat_byte(0x01), &[], 0), None);
        assert_eq!(recover_code_matching(B256::repeat_byte(0x01), &[], 5), None);
    }

    /// The fast path stays O(1) hashes even for a large blob: a hit at the
    /// recorded length returns without scanning.
    #[test]
    fn large_blob_fast_path() {
        let code: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let key = alloy::primitives::keccak256(&code);
        let blob = blob_with(&code, 4096);
        assert_eq!(
            recover_code_matching(key, &blob, code.len()),
            Some(code.clone())
        );
    }
}
