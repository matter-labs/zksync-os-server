//! The node's own durable store of finality certificates.
//!
//! Consensus keeps certificates in its engine's archives, encoded by the pinned
//! consensus library — a format that may change with any library upgrade. This store
//! is the node's sovereign copy: every finalization certificate, converted to the
//! node's own wire format ([`FinalityCertificate`]) the moment it is observed, keyed
//! for the two questions the chain will ever ask — "the certificate for this block
//! digest" and "the certificate for this height". With it, the engine's archives are
//! a rebuildable cache; without it, a library upgrade could strand the one artifact
//! that later makes finality externally provable.
//!
//! Two independent writers feed it, on different threads, in no guaranteed order:
//! the consensus activity observer writes certificates (it sees the certificate and
//! the block digest, but no height), and the execution environment's commit path
//! writes the height→digest index (it sees the block, but no certificate). Both
//! writes are idempotent, so at-least-once delivery and restarts are absorbed.
//!
//! The *certified watermark* joins the two streams: the highest height H such that
//! every height 1..=H has an indexed digest whose certificate is present. It only
//! advances — a gap in either stream stalls it, which makes it an honest
//! end-to-end health gauge (surfaced in `/status`).

use std::path::Path;
use std::sync::Mutex;
use tokio::sync::watch;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;
use zksync_os_wire::FinalityCertificate;

#[derive(Clone, Copy, Debug)]
pub enum FinalityCF {
    /// Certificate by the block's consensus digest (32 bytes).
    Certificates,
    /// Consensus digest by block height (u64, big-endian — RocksDB orders keys).
    HeightIndex,
    /// Small bookkeeping values; currently only the certified watermark.
    Meta,
}

impl NamedColumnFamily for FinalityCF {
    const DB_NAME: &'static str = "finality";
    const ALL: &'static [Self] = &[
        FinalityCF::Certificates,
        FinalityCF::HeightIndex,
        FinalityCF::Meta,
    ];

    fn name(&self) -> &'static str {
        match self {
            FinalityCF::Certificates => "certificates",
            FinalityCF::HeightIndex => "height_index",
            FinalityCF::Meta => "meta",
        }
    }
}

const WATERMARK_KEY: &[u8] = b"certified_watermark";
const OBSERVED_ROUND_KEY: &[u8] = b"highest_observed_round";
const ERA_KEY: &[u8] = b"consensus_era";

pub struct FinalityStore {
    db: RocksDB<FinalityCF>,
    /// Serializes watermark advances: both writers try to advance after their write,
    /// and a read-modify-write race would otherwise lose ground (it would self-heal
    /// on the next write, but there is no reason to allow the gap).
    watermark_lock: Mutex<()>,
    /// Broadcasts watermark advances to the status surface.
    watermark_watch: watch::Sender<Option<u64>>,
}

impl FinalityStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = RocksDB::new(path)?;
        let store = Self {
            db,
            watermark_lock: Mutex::new(()),
            watermark_watch: watch::channel(None).0,
        };
        // Re-publish the persisted watermark so the status surface is right from the
        // first observation after a restart. (`send_replace`, not `send`: a plain
        // send is dropped while nobody subscribes yet, and late subscribers would
        // see the initial `None` forever.)
        store
            .watermark_watch
            .send_replace(store.certified_watermark()?);
        Ok(store)
    }

    /// Watch of the certified watermark (see the module docs). `None` until height 1
    /// is certified.
    pub fn watermark_subscription(&self) -> watch::Receiver<Option<u64>> {
        self.watermark_watch.subscribe()
    }

    /// Stores a certificate under its block digest. Idempotent; called by the
    /// consensus activity observer at every finalization (including re-observations
    /// after a restart).
    pub fn put_certificate(&self, certificate: &FinalityCertificate) -> anyhow::Result<()> {
        use commonware_codec::{EncodeSize, Write as _};
        let mut encoded = Vec::with_capacity(certificate.encode_size());
        certificate.write(&mut encoded);
        let mut batch = self.db.new_write_batch();
        batch.put_cf(
            FinalityCF::Certificates,
            &certificate.block_digest,
            &encoded,
        );
        self.db.write(batch)?;
        self.advance_watermark()
    }

    /// Records which digest was finalized at `height`. Idempotent; called by the
    /// execution environment for every delivered finalized block.
    pub fn index_height(&self, height: u64, digest: [u8; 32]) -> anyhow::Result<()> {
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::HeightIndex, &height.to_be_bytes(), &digest);
        self.db.write(batch)?;
        self.advance_watermark()
    }

    /// The consensus era this chain runs: the digest of the consensus genesis block
    /// (which commits to the anchor height and the anchored block's hash). Written at
    /// the first consensus start; startup refuses to proceed when the configured
    /// anchor derives a different digest over non-fresh consensus state — mixing
    /// consensus eras (e.g. re-migrating after a rollback without clearing the old
    /// era's engine state) must be impossible to do by accident.
    pub fn consensus_era(&self) -> anyhow::Result<Option<[u8; 32]>> {
        let Some(bytes) = self.db.get_cf(FinalityCF::Meta, ERA_KEY)? else {
            return Ok(None);
        };
        bytes
            .try_into()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("stored consensus era is not a 32-byte digest"))
    }

    /// `anchor_height` floors the certified watermark: heights at or below the
    /// consensus anchor are pre-consensus history — finalized by the era cutover
    /// itself, with no certificates to wait for. Without the floor, a migrated
    /// chain's watermark would wait forever for certificates that can never exist.
    pub fn record_consensus_era(
        &self,
        genesis_digest: [u8; 32],
        anchor_height: u64,
    ) -> anyhow::Result<()> {
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::Meta, ERA_KEY, &genesis_digest);
        self.db.write(batch)?;
        {
            let _guard = self.watermark_lock.lock().unwrap();
            if anchor_height > 0 && self.certified_watermark()?.unwrap_or(0) < anchor_height {
                let mut batch = self.db.new_write_batch();
                batch.put_cf(
                    FinalityCF::Meta,
                    WATERMARK_KEY,
                    &anchor_height.to_be_bytes(),
                );
                self.db.write(batch)?;
                self.watermark_watch.send_replace(Some(anchor_height));
            }
        }
        // Certificates observed before the era was recorded may already continue
        // past the floor.
        self.advance_watermark()
    }

    /// Records the highest consensus round this validator has *seen* — an upper
    /// bound on the views it could have signed votes in. This is the recovery floor
    /// for the one scenario the consensus engine's own journal cannot cover: the
    /// journal becoming unreadable (a consensus-library upgrade breaking its format,
    /// disk loss). The restart runbook then requires the live committee's view to be
    /// beyond this floor before the validator starts voting again — seconds of
    /// waiting instead of an equivocation risk. Deliberately not enforced inside the
    /// engine (vote admission is the consensus library's job; this is operator
    /// input), and deliberately an over-approximation (observing a round is cheaper
    /// and safer to track than proving which rounds were signed).
    pub fn note_observed_round(&self, epoch: u64, view: u64) -> anyhow::Result<()> {
        let _guard = self.watermark_lock.lock().unwrap();
        if let Some((seen_epoch, seen_view)) = self.highest_observed_round()?
            && (epoch, view) <= (seen_epoch, seen_view)
        {
            return Ok(());
        }
        let mut value = Vec::with_capacity(16);
        value.extend_from_slice(&epoch.to_be_bytes());
        value.extend_from_slice(&view.to_be_bytes());
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::Meta, OBSERVED_ROUND_KEY, &value);
        self.db.write(batch)?;
        Ok(())
    }

    /// The highest `(epoch, view)` ever observed by this validator, if any.
    pub fn highest_observed_round(&self) -> anyhow::Result<Option<(u64, u64)>> {
        let Some(bytes) = self.db.get_cf(FinalityCF::Meta, OBSERVED_ROUND_KEY)? else {
            return Ok(None);
        };
        anyhow::ensure!(bytes.len() == 16, "stored observed round is not two u64s");
        let epoch = u64::from_be_bytes(bytes[..8].try_into().expect("checked length"));
        let view = u64::from_be_bytes(bytes[8..].try_into().expect("checked length"));
        Ok(Some((epoch, view)))
    }

    pub fn certificate_by_digest(
        &self,
        digest: &[u8; 32],
    ) -> anyhow::Result<Option<FinalityCertificate>> {
        use commonware_codec::Read as _;
        let Some(bytes) = self.db.get_cf(FinalityCF::Certificates, digest)? else {
            return Ok(None);
        };
        let certificate = FinalityCertificate::read_cfg(&mut bytes.as_slice(), &())
            .map_err(|err| anyhow::anyhow!("stored certificate does not decode: {err}"))?;
        Ok(Some(certificate))
    }

    pub fn certificate_by_height(
        &self,
        height: u64,
    ) -> anyhow::Result<Option<FinalityCertificate>> {
        let Some(digest) = self
            .db
            .get_cf(FinalityCF::HeightIndex, &height.to_be_bytes())?
        else {
            return Ok(None);
        };
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored height index is not a 32-byte digest"))?;
        self.certificate_by_digest(&digest)
    }

    /// The highest height H such that every height `1..=H` has a certificate (via
    /// its indexed digest). `None` before height 1 is certified.
    pub fn certified_watermark(&self) -> anyhow::Result<Option<u64>> {
        let Some(bytes) = self.db.get_cf(FinalityCF::Meta, WATERMARK_KEY)? else {
            return Ok(None);
        };
        let bytes: [u8; 8] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored watermark is not a u64"))?;
        Ok(Some(u64::from_be_bytes(bytes)))
    }

    /// Advances the certified watermark over every consecutive height that now has a
    /// certificate. Amortized O(1): each height is walked over once, ever.
    fn advance_watermark(&self) -> anyhow::Result<()> {
        let _guard = self.watermark_lock.lock().unwrap();
        let mut watermark = self.certified_watermark()?;
        let mut advanced = false;
        while self
            .certificate_by_height(watermark.unwrap_or(0) + 1)?
            .is_some()
        {
            watermark = Some(watermark.unwrap_or(0) + 1);
            advanced = true;
        }
        if advanced {
            let mut batch = self.db.new_write_batch();
            batch.put_cf(
                FinalityCF::Meta,
                WATERMARK_KEY,
                &watermark.expect("advanced implies a value").to_be_bytes(),
            );
            self.db.write(batch)?;
            self.watermark_watch.send_replace(watermark);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_wire::SignatureScheme;

    fn certificate(view: u64, digest: [u8; 32]) -> FinalityCertificate {
        FinalityCertificate {
            scheme: SignatureScheme::Bls12381Multisig,
            epoch: 0,
            view,
            block_digest: digest,
            committee_size: 4,
            signers: FinalityCertificate::bitmap_from_positions(4, &[0, 1, 2]),
            signature: vec![0xAB; 96],
        }
    }

    #[test]
    fn watermark_advances_only_over_contiguous_certified_heights() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");

        // Certificate and index arriving in either order; height 2 missing its
        // certificate holds the watermark at 1 until it shows up.
        store.index_height(1, [1; 32]).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), None);
        store
            .put_certificate(&certificate(10, [1; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(1));

        store
            .put_certificate(&certificate(11, [2; 32]))
            .expect("write");
        store
            .put_certificate(&certificate(12, [3; 32]))
            .expect("write");
        store.index_height(3, [3; 32]).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(1));
        store.index_height(2, [2; 32]).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(3));

        // Idempotent redelivery changes nothing.
        store.index_height(2, [2; 32]).expect("write");
        store
            .put_certificate(&certificate(11, [2; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(3));

        // Lookups answer by digest and by height alike.
        let by_height = store
            .certificate_by_height(2)
            .expect("read")
            .expect("present");
        assert_eq!(by_height.view, 11);
        assert!(store.certificate_by_height(4).expect("read").is_none());
    }

    #[test]
    fn observed_round_floor_is_monotone_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = FinalityStore::open(dir.path()).expect("open");
            assert_eq!(store.highest_observed_round().expect("read"), None);
            store.note_observed_round(0, 5).expect("write");
            // A lower round never regresses the floor.
            store.note_observed_round(0, 3).expect("write");
            assert_eq!(store.highest_observed_round().expect("read"), Some((0, 5)));
            // A later epoch advances it regardless of the view number.
            store.note_observed_round(1, 1).expect("write");
            assert_eq!(store.highest_observed_round().expect("read"), Some((1, 1)));
        }
        let store = FinalityStore::open(dir.path()).expect("reopen");
        assert_eq!(store.highest_observed_round().expect("read"), Some((1, 1)));
    }

    #[test]
    fn era_recording_floors_the_watermark_at_the_anchor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");
        // A migrated chain: heights 1..=20 are pre-consensus, no certificates exist
        // for them and none ever will.
        store.record_consensus_era([7; 32], 20).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(20));
        // The first consensus-era block certifies normally on top of the floor.
        store.index_height(21, [1; 32]).expect("write");
        store
            .put_certificate(&certificate(1, [1; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(21));
        // Re-recording the same era never regresses anything.
        store.record_consensus_era([7; 32], 20).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(21));
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = FinalityStore::open(dir.path()).expect("open");
            store
                .put_certificate(&certificate(7, [9; 32]))
                .expect("write");
            store.index_height(1, [9; 32]).expect("write");
            assert_eq!(store.certified_watermark().expect("read"), Some(1));
        }
        let store = FinalityStore::open(dir.path()).expect("reopen");
        assert_eq!(store.certified_watermark().expect("read"), Some(1));
        assert_eq!(
            store
                .certificate_by_height(1)
                .expect("read")
                .expect("present")
                .view,
            7
        );
        assert_eq!(*store.watermark_subscription().borrow(), Some(1));
    }
}
