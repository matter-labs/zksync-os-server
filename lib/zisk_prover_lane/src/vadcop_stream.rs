//! Server-side parsing of a serialized non-minimal ZiSK `vadcop_final`
//! proof stream.
//!
//! In aggregated mode the daemon proves each batch WITHOUT the PLONK wrap
//! and submits the raw proof stream (`zisk_common::Proof::get_proof_bytes()`
//! layout, ZiSK v1.2.0-alpha) so the aggregator guest can later verify it
//! in-zkVM. This module only parses: it validates the stream shape and
//! extracts the values checked per batch — the inner guest's program VK
//! (tripwire) and the committed batch public input. The STARK itself is
//! verified separately by the native verifier on the submit path.
//!
//! Stream layout (u64 LE words):
//!
//! ```text
//! [minimal=0][n_publics=69][is_vadcop_final_proof=1][program_vk(4)]
//! [publics(64)][proof body(46_078)][vadcop_vk(4)]
//! ```
//!
//! `publics[0..8]` carry the STF guest's batch-commitment u32 words (one
//! u32 per u64 word, packed little-endian by `ziskos::io::commit_slice`).
//! The same words reach the PLONK wire public values widened to u64, so
//! the commitment lands in bytes `[32..96]` there, four significant bytes
//! per eight-byte slot.
//!
//! The layout mirrors `zksync-os-zisk/guest-aggregator/src/lib.rs`, which
//! is the parser the aggregator guest itself runs; the two must agree.

use alloy::primitives::B256;

/// Words preceding the publics in a serialized proof: `[minimal][n_publics]`.
pub const VADCOP_HEADER_WORDS: usize = 2;
/// u64 words in the leading `is_vadcop_final_proof` public. The non-minimal
/// vadcop_final circuit emits it at public index 0, ahead of the program VK.
pub const VADCOP_LEAF_FLAG_WORDS: usize = 1;
/// The `is_vadcop_final_proof` value of a leaf proof — one batch, proven by
/// the STF guest. A fold output of ZiSK's own recurser carries 0, so this is
/// the value that keeps a folded range out of a per-batch position.
pub const VADCOP_LEAF_FLAG: u64 = 1;
/// u64 words in the guest-ELF ROM root (program VK).
pub const VADCOP_PROGRAM_VK_WORDS: usize = 4;
/// u64 words in the publics region.
pub const VADCOP_PUBLICS_WORDS: usize = 64;
/// Publics words carrying the STF guest's batch commitment. Only the
/// stream-fixture builder needs it; the parser derives the commitment from the
/// 32-byte layout directly.
#[cfg(any(test, feature = "test-support"))]
pub const VADCOP_COMMITMENT_WORDS: usize = 8;
/// u64 words in the vadcop-final verification key trailing the stream.
pub const VADCOP_VK_WORDS: usize = 4;
/// u64 words in a non-minimal `vadcop_final` proof body under the pinned
/// pil2-proofman recursive setup, Poseidon1 hash family — the family
/// `ziskos::zisklib::verify_zisk_proof` fixes. Must match
/// `zksync-os-zisk-guest-aggregator::VADCOP_FINAL_BODY_WORDS`, which is
/// asserted against the real `proofman-verifier` crate at the pinned version.
pub const VADCOP_FINAL_BODY_WORDS: usize = 46_078;
/// Expected `n_publics` header word: leaf flag + program VK + publics.
pub const VADCOP_EXPECTED_N_PUBLICS: u64 =
    (VADCOP_LEAF_FLAG_WORDS + VADCOP_PROGRAM_VK_WORDS + VADCOP_PUBLICS_WORDS) as u64;

/// Total u64 words in a serialized non-minimal proof stream.
pub const VADCOP_STREAM_WORDS: usize = VADCOP_HEADER_WORDS
    + VADCOP_LEAF_FLAG_WORDS
    + VADCOP_PROGRAM_VK_WORDS
    + VADCOP_PUBLICS_WORDS
    + VADCOP_FINAL_BODY_WORDS
    + VADCOP_VK_WORDS;
/// Total bytes in a serialized non-minimal proof stream (369_224).
pub const ZISK_VADCOP_STREAM_BYTES: usize = VADCOP_STREAM_WORDS * 8;

/// The public data of a validated `vadcop_final` proof stream, in the same
/// serialization the rest of the stack uses:
/// - VKs as the 32-byte big-endian value (the four u64 limbs, big-endian,
///   in order) — matching the compiled ZiSK manifest VK format and bytes
///   `[0..32]` / `[544..576]` of the PLONK wire public values.
/// - The commitment as the 32 bytes the STF guest committed (u32 words
///   packed little-endian), which the PLONK wire public values carry
///   widened to u64 in bytes `[32..96]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VadcopStreamPublics {
    pub program_vk: B256,
    pub vadcop_vk: B256,
    pub commitment: B256,
}

fn word_at(bytes: &[u8], index: usize) -> u64 {
    u64::from_le_bytes(bytes[index * 8..(index + 1) * 8].try_into().unwrap())
}

/// Render 4 u64 VK limbs as the 32-byte big-endian wire value.
fn vk_words_be(bytes: &[u8], first_word: usize) -> B256 {
    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_exact_mut(8).enumerate() {
        chunk.copy_from_slice(&word_at(bytes, first_word + i).to_be_bytes());
    }
    B256::from(out)
}

/// Validate the stream shape and extract its public data.
///
/// Shape checks: exact byte length, non-minimal flag, publics count, leaf
/// flag. Cryptographic verification of the STARK happens only inside the
/// aggregator guest; the caller must still validate the extracted
/// commitment against the batch metadata and the program VK against the
/// expected guest build.
///
/// INVARIANT: only a leaf proof is accepted. A fold output of ZiSK's own
/// recurser is a valid STARK over the same keys, and its publics sit at the
/// same offsets, so `is_vadcop_final_proof` is the only thing that separates
/// one batch's proof from a folded range standing in for it.
pub fn parse_vadcop_final_stream(bytes: &[u8]) -> Result<VadcopStreamPublics, String> {
    if bytes.len() != ZISK_VADCOP_STREAM_BYTES {
        return Err(format!(
            "vadcop_final stream must be exactly {ZISK_VADCOP_STREAM_BYTES} bytes, got {}",
            bytes.len()
        ));
    }
    let minimal = word_at(bytes, 0);
    if minimal != 0 {
        return Err(format!(
            "minimal vadcop_final proofs are not accepted (flag word {minimal})"
        ));
    }
    let n_publics = word_at(bytes, 1);
    if n_publics != VADCOP_EXPECTED_N_PUBLICS {
        return Err(format!(
            "n_publics must be {VADCOP_EXPECTED_N_PUBLICS}, got {n_publics}"
        ));
    }

    let leaf_flag = word_at(bytes, VADCOP_HEADER_WORDS);
    if leaf_flag != VADCOP_LEAF_FLAG {
        return Err(format!(
            "is_vadcop_final_proof is {leaf_flag}, expected leaf proof ({VADCOP_LEAF_FLAG})"
        ));
    }

    let program_vk = vk_words_be(bytes, VADCOP_HEADER_WORDS + VADCOP_LEAF_FLAG_WORDS);
    let vadcop_vk = vk_words_be(bytes, VADCOP_STREAM_WORDS - VADCOP_VK_WORDS);

    // publics words 0..8, one u32 payload per u64 word, packed LE — the
    // `as u32` truncation matches ziskos's `PublicValues::new_from_u64`.
    let publics_start = VADCOP_HEADER_WORDS + VADCOP_LEAF_FLAG_WORDS + VADCOP_PROGRAM_VK_WORDS;
    let mut commitment = [0u8; 32];
    for (i, chunk) in commitment.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&(word_at(bytes, publics_start + i) as u32).to_le_bytes());
    }

    Ok(VadcopStreamPublics {
        program_vk,
        vadcop_vk,
        commitment: B256::from(commitment),
    })
}

/// A structurally exact (cryptographically invalid) stream for unit tests:
/// given VK limbs and a 32-byte commitment, produce a stream that parses back
/// to exactly those values. Exposed for tests in other crates via the
/// `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub fn synthetic_stream(
    program_vk: [u64; VADCOP_PROGRAM_VK_WORDS],
    vadcop_vk: [u64; VADCOP_VK_WORDS],
    commitment: [u8; 32],
) -> Vec<u8> {
    let mut words: Vec<u64> = Vec::with_capacity(VADCOP_STREAM_WORDS);
    words.push(0); // non-minimal
    words.push(VADCOP_EXPECTED_N_PUBLICS);
    words.push(VADCOP_LEAF_FLAG);
    words.extend_from_slice(&program_vk);
    let mut publics = [0u64; VADCOP_PUBLICS_WORDS];
    for (i, p) in publics.iter_mut().take(VADCOP_COMMITMENT_WORDS).enumerate() {
        *p = u32::from_le_bytes(commitment[i * 4..(i + 1) * 4].try_into().unwrap()) as u64;
    }
    words.extend_from_slice(&publics);
    words.extend((0..VADCOP_FINAL_BODY_WORDS).map(|i| (i as u64) % (1 << 31)));
    words.extend_from_slice(&vadcop_vk);
    debug_assert_eq!(words.len(), VADCOP_STREAM_WORDS);

    let mut bytes = Vec::with_capacity(ZISK_VADCOP_STREAM_BYTES);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::synthetic_stream;
    use super::*;

    const PROGRAM_VK: [u64; 4] = [1, 2, 3, 4];
    const VADCOP_VK: [u64; 4] = [5, 6, 7, 8];

    #[test]
    fn parses_well_shaped_stream() {
        let commitment = [0x42u8; 32];
        let stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, commitment);
        assert_eq!(stream.len(), ZISK_VADCOP_STREAM_BYTES);

        let publics = parse_vadcop_final_stream(&stream).expect("parses");
        assert_eq!(publics.commitment, B256::from(commitment));
        // VK limbs rendered big-endian, in order — the wire/config form.
        let mut expected_vk = [0u8; 32];
        for (i, chunk) in expected_vk.chunks_exact_mut(8).enumerate() {
            chunk.copy_from_slice(&PROGRAM_VK[i].to_be_bytes());
        }
        assert_eq!(publics.program_vk, B256::from(expected_vk));
        let mut expected_vvk = [0u8; 32];
        for (i, chunk) in expected_vvk.chunks_exact_mut(8).enumerate() {
            chunk.copy_from_slice(&VADCOP_VK[i].to_be_bytes());
        }
        assert_eq!(publics.vadcop_vk, B256::from(expected_vvk));
    }

    #[test]
    fn rejects_wrong_length() {
        let stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, [0u8; 32]);
        let err = parse_vadcop_final_stream(&stream[..stream.len() - 8]).unwrap_err();
        assert!(err.contains("exactly"), "{err}");
        // The 768-byte PLONK proof size is the classic mode mismatch.
        let err = parse_vadcop_final_stream(&[0u8; 768]).unwrap_err();
        assert!(err.contains("768"), "{err}");
    }

    #[test]
    fn rejects_minimal_flag_and_bad_publics_count() {
        let mut stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, [0u8; 32]);
        stream[0] = 1;
        let err = parse_vadcop_final_stream(&stream).unwrap_err();
        assert!(err.contains("minimal"), "{err}");

        let mut stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, [0u8; 32]);
        stream[8] = 68;
        let err = parse_vadcop_final_stream(&stream).unwrap_err();
        assert!(err.contains("n_publics"), "{err}");
    }

    /// A fold output of ZiSK's recurser has the leaf flag clear and is
    /// otherwise well shaped, so this check is what keeps a folded range out
    /// of a per-batch position.
    #[test]
    fn rejects_a_stream_that_is_not_a_leaf_proof() {
        let mut stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, [0u8; 32]);
        stream[VADCOP_HEADER_WORDS * 8] = 0;
        let err = parse_vadcop_final_stream(&stream).unwrap_err();
        assert!(err.contains("is_vadcop_final_proof"), "{err}");
    }

    #[test]
    fn commitment_truncates_words_to_u32() {
        let mut stream = synthetic_stream(PROGRAM_VK, VADCOP_VK, [0x11u8; 32]);
        // Poison the high half of the first commitment word; the parser
        // must ignore it exactly like the guest's frame.commitment().
        let first_publics_byte =
            (VADCOP_HEADER_WORDS + VADCOP_LEAF_FLAG_WORDS + VADCOP_PROGRAM_VK_WORDS) * 8;
        stream[first_publics_byte + 4] = 0xDE;
        let publics = parse_vadcop_final_stream(&stream).expect("parses");
        assert_eq!(publics.commitment, B256::from([0x11u8; 32]));
    }
}
