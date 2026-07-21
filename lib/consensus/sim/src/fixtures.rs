//! Consensus-storage fixtures: freeze one deterministic run's storage, reopen it later.
//!
//! The replay gate (see `tests/replay_gate.rs`) pins a cluster's entire consensus-side
//! storage — engine vote journals, marshal's block/finalization archives, its caches and
//! processed-height marker — as a committed binary fixture, and proves on every run that
//! the *current* stack can reopen that storage and resume finalizing. Same-session
//! restarts are covered by the crash/restart scenarios; the fixture covers the axis they
//! cannot: yesterday's bytes under today's code (commonware upgrades, our own config and
//! stack changes).
//!
//! The deterministic runtime's storage has no partition-enumeration API, so capture
//! works from a *candidate list* of partition names derived from the stack's known
//! layout ([`candidate_partitions`]). Two properties keep that honest:
//!
//! - the generator asserts the load-bearing partitions were captured non-empty
//!   ([`StorageFixture::assert_load_bearing_partitions`]), so a partition-name change
//!   breaks fixture *regeneration* loudly rather than silently shrinking coverage;
//! - the gate asserts the restored cluster resumes *above* the fixture's height, so a
//!   restored-but-unread fixture (e.g. the stack starts reading different partitions)
//!   fails the gate rather than passing as an accidental fresh start.
//!
//! Blocks in the fixture are mock-execution blocks: the consensus-side encodings this
//! gate pins do not depend on the STF, and the block *wire* formats are pinned
//! byte-for-byte by the `lib/wire` goldens independently.

use commonware_runtime::{Blob as _, Storage as _, deterministic};
use zksync_os_consensus_core::engine_partition;

/// One partition's captured content: `(partition name, [(blob name, bytes)])`.
pub type PartitionDump = (String, Vec<(Vec<u8>, Vec<u8>)>);

/// A frozen consensus-storage state plus the run parameters that produced it.
/// The parameters are stored so the gate can prove it is replaying under the
/// same geometry (keys are minted from the seed; epochs shape the partitions).
pub struct StorageFixture {
    pub seed: u64,
    pub num_validators: u32,
    pub epoch_length: u64,
    /// The committed height every validator had reached when the state was frozen.
    pub height: u64,
    pub partitions: Vec<PartitionDump>,
    /// Each validator's committed chain (encoded blocks, height order): the mock
    /// stand-in for the node's write-ahead log, which in production survives a
    /// restart *alongside* consensus storage. Restoring partitions over empty
    /// environments would model a state no healthy node is ever in.
    pub chains: Vec<Vec<Vec<u8>>>,
}

const MAGIC: &[u8] = b"ZKOS-CONSENSUS-STORAGE-FIXTURE-V1\n";

impl StorageFixture {
    /// Serializes the fixture. The format is a plain length-prefixed dump —
    /// deliberately dependency-free and versioned by [`MAGIC`]; a format change
    /// is a new magic string and a new fixture file, never a silent rewrite.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&self.num_validators.to_le_bytes());
        out.extend_from_slice(&self.epoch_length.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&(self.partitions.len() as u64).to_le_bytes());
        for (partition, blobs) in &self.partitions {
            write_bytes(&mut out, partition.as_bytes());
            out.extend_from_slice(&(blobs.len() as u64).to_le_bytes());
            for (name, data) in blobs {
                write_bytes(&mut out, name);
                write_bytes(&mut out, data);
            }
        }
        out.extend_from_slice(&(self.chains.len() as u64).to_le_bytes());
        for chain in &self.chains {
            out.extend_from_slice(&(chain.len() as u64).to_le_bytes());
            for block in chain {
                write_bytes(&mut out, block);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Self {
        let mut cursor = Cursor { bytes, position: 0 };
        assert_eq!(
            cursor.take(MAGIC.len()),
            MAGIC,
            "not a consensus-storage fixture (or a newer format — regenerate deliberately)"
        );
        let seed = cursor.u64();
        let num_validators = cursor.u32();
        let epoch_length = cursor.u64();
        let height = cursor.u64();
        let partition_count = cursor.u64();
        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            let partition = String::from_utf8(cursor.bytes_field().to_vec())
                .expect("partition names are UTF-8");
            let blob_count = cursor.u64();
            let mut blobs = Vec::with_capacity(blob_count as usize);
            for _ in 0..blob_count {
                let name = cursor.bytes_field().to_vec();
                let data = cursor.bytes_field().to_vec();
                blobs.push((name, data));
            }
            partitions.push((partition, blobs));
        }
        let chain_count = cursor.u64();
        let mut chains = Vec::with_capacity(chain_count as usize);
        for _ in 0..chain_count {
            let block_count = cursor.u64();
            let mut chain = Vec::with_capacity(block_count as usize);
            for _ in 0..block_count {
                chain.push(cursor.bytes_field().to_vec());
            }
            chains.push(chain);
        }
        assert_eq!(cursor.position, bytes.len(), "trailing bytes in fixture");
        Self {
            seed,
            num_validators,
            epoch_length,
            height,
            partitions,
            chains,
        }
    }

    /// The generator-side honesty check: every validator must have contributed the
    /// partitions a restart actually reads. A miss means [`candidate_partitions`]
    /// went stale against the stack's real layout — fail regeneration, don't ship
    /// a hollow fixture.
    pub fn assert_load_bearing_partitions(&self, storage_prefix: &str) {
        for index in 0..self.num_validators {
            let prefix = format!("{storage_prefix}-{index}");
            for required in [
                format!("{prefix}-blocks-value"),
                format!("{prefix}-finalizations-value"),
                format!("{prefix}-application-metadata"),
            ] {
                assert!(
                    self.partitions
                        .iter()
                        .any(|(name, blobs)| *name == required && !blobs.is_empty()),
                    "fixture is missing `{required}` — the candidate partition list \
                     no longer matches the stack's storage layout",
                );
            }
            assert!(
                self.partitions.iter().any(|(name, blobs)| name
                    .starts_with(&format!("{prefix}-engine-epoch-"))
                    && !blobs.is_empty()),
                "fixture holds no engine journal for validator {index}",
            );
        }
    }
}

/// Every partition name the consensus stack may have created for `prefix` with
/// epochs `0..=max_epoch`. Derived from the stack's storage layout: our own
/// engine journals and archive partitions, plus the partitions marshal derives
/// internally from its prefix (cache archives and the processed-height marker).
pub fn candidate_partitions(prefix: &str, max_epoch: u64) -> Vec<String> {
    let mut names = vec![
        format!("{prefix}-finalizations-key"),
        format!("{prefix}-finalizations-value"),
        format!("{prefix}-blocks-key"),
        format!("{prefix}-blocks-value"),
        format!("{prefix}-application-metadata"),
        format!("{prefix}-cache-metadata"),
    ];
    for epoch in 0..=max_epoch {
        names.push(engine_partition(prefix, epoch));
        for kind in [
            "verified",
            "notarized",
            "certified",
            "notarizations",
            "finalizations",
        ] {
            names.push(format!("{prefix}-cache-cache-{epoch}-{kind}-key"));
            names.push(format!("{prefix}-cache-cache-{epoch}-{kind}-value"));
        }
    }
    names
}

/// Reads every existing candidate partition out of the context's storage.
/// Missing candidates are skipped (not every epoch starts, caches prune);
/// [`StorageFixture::assert_load_bearing_partitions`] guards against the
/// skip-everything failure mode.
pub async fn capture_partitions(
    context: &deterministic::Context,
    candidates: &[String],
) -> Vec<PartitionDump> {
    let mut partitions = Vec::new();
    for partition in candidates {
        let Ok(blob_names) = context.scan(partition).await else {
            continue;
        };
        let mut blobs = Vec::with_capacity(blob_names.len());
        for name in blob_names {
            let (blob, size) = context
                .open(partition, &name)
                .await
                .expect("scan listed the blob");
            let data = if size == 0 {
                Vec::new()
            } else {
                blob.read_at(0, size as usize)
                    .await
                    .expect("blob read within its size")
                    .coalesce()
                    .as_ref()
                    .to_vec()
            };
            blobs.push((name, data));
        }
        partitions.push((partition.clone(), blobs));
    }
    partitions
}

/// Writes a fixture's partitions into a (fresh) context's storage, byte-exactly.
pub async fn restore_partitions(context: &deterministic::Context, partitions: &[PartitionDump]) {
    for (partition, blobs) in partitions {
        for (name, data) in blobs {
            let (blob, size) = context
                .open(partition, name)
                .await
                .expect("restoring into fresh storage");
            assert_eq!(size, 0, "restore target must be fresh storage");
            blob.write_at(0, data.clone()).await.expect("restore write");
            blob.sync().await.expect("restore sync");
        }
    }
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> &'a [u8] {
        let slice = &self.bytes[self.position..self.position + len];
        self.position += len;
        slice
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().expect("8 bytes"))
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().expect("4 bytes"))
    }

    fn bytes_field(&mut self) -> &'a [u8] {
        let len = self.u64() as usize;
        self.take(len)
    }
}
