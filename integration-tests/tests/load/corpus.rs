//! Bench-only (TEMPORARY): a pre-built on-disk transaction corpus so load tests stream ready-made
//! transactions instead of constructing/signing them in the hot loop (which caps the direct-injection
//! pipeline at ~1.24M TPS via `build_direct_tx`'s keccak). One file per signer, generated once on
//! first use and reused afterwards.
//!
//! File layout: `[MAGIC u32][VERSION u32][fingerprint u64][count u64]` then `count` records, each
//! `[len u32][bytes]`. The `fingerprint` captures the generation parameters (chain id, scheme, …) so a
//! stale or mismatched file is regenerated automatically.

use rayon::prelude::*;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: u32 = 0x5A4B5458; // "ZKTX"
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 8 + 8;
/// Records generated + written per streaming chunk (bounds peak memory: ~chunk * record size).
const GEN_CHUNK: u64 = 1 << 20;

/// Corpus directory for a `family` of signer files. Override the base with `LOADTEST_CORPUS_DIR`
/// (e.g. point at fast local disk on a rented box); default is gitignored (`db/*`).
pub fn corpus_dir(family: &str) -> PathBuf {
    let base = std::env::var("LOADTEST_CORPUS_DIR")
        .unwrap_or_else(|_| format!("{}/db/bench-txs", env!("CARGO_MANIFEST_DIR")));
    PathBuf::from(base).join(family)
}

/// Path of the per-signer corpus file `signer-<index>.bin` within `family`.
pub fn signer_file(family: &str, index: usize) -> PathBuf {
    corpus_dir(family).join(format!("signer-{index}.bin"))
}

/// Convenience: fold any number of `Hash` params into a u64 corpus fingerprint.
pub fn fingerprint(parts: &[u64]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    VERSION.hash(&mut h);
    parts.hash(&mut h);
    h.finish()
}

/// Ensure `path` holds at least `count` records produced with `fingerprint`. Regenerates (atomically,
/// via a temp file + rename) when the file is missing, the header's magic/version/fingerprint differs,
/// or it holds fewer than `count` records. `gen(record_index) -> bytes` is invoked in parallel.
pub fn ensure_corpus<F>(
    path: &Path,
    count: u64,
    fingerprint: u64,
    generate: F,
) -> anyhow::Result<()>
where
    F: Fn(u64) -> Vec<u8> + Sync,
{
    if is_valid(path, count, fingerprint)? {
        tracing::info!(path = %path.display(), count, "corpus present, reusing");
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::with_capacity(8 << 20, File::create(&tmp)?);
    w.write_all(&MAGIC.to_le_bytes())?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&fingerprint.to_le_bytes())?;
    w.write_all(&count.to_le_bytes())?;

    tracing::warn!(path = %path.display(), count, "generating tx corpus (one-time); this may take a while");
    let started = std::time::Instant::now();
    let mut next = 0u64;
    while next < count {
        let end = (next + GEN_CHUNK).min(count);
        // Parallel-generate the chunk in order, then write it; peak memory is one chunk.
        let chunk: Vec<Vec<u8>> = (next..end).into_par_iter().map(&generate).collect();
        for rec in &chunk {
            w.write_all(&(rec.len() as u32).to_le_bytes())?;
            w.write_all(rec)?;
        }
        next = end;
        tracing::info!(
            path = %path.display(),
            generated = next,
            count,
            elapsed = ?started.elapsed(),
            "corpus generation progress"
        );
    }
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, path)?;
    tracing::info!(path = %path.display(), count, elapsed = ?started.elapsed(), "corpus generated");
    Ok(())
}

fn is_valid(path: &Path, count: u64, fingerprint: u64) -> anyhow::Result<bool> {
    let Ok(mut f) = File::open(path) else {
        return Ok(false);
    };
    let mut hdr = [0u8; HEADER_LEN];
    if f.read_exact(&mut hdr).is_err() {
        return Ok(false);
    }
    let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    let fp = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
    let cnt = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    Ok(magic == MAGIC && version == VERSION && fp == fingerprint && cnt >= count)
}

/// Sequential reader over a corpus file's records. Cheap per-record (buffered read, no decode).
pub struct CorpusReader {
    reader: BufReader<File>,
}

impl CorpusReader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut f = File::open(path)?;
        let mut hdr = [0u8; HEADER_LEN];
        f.read_exact(&mut hdr)?; // skip past header; already validated by `ensure_corpus`
        Ok(Self {
            reader: BufReader::with_capacity(8 << 20, f),
        })
    }

    /// Next record's bytes, or `None` at end of file.
    pub fn next_record(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf)?;
        Ok(Some(buf))
    }
}
