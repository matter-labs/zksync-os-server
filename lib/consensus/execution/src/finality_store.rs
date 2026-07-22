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
//! The *certified watermark* joins the two streams: the highest height H that is
//! *covered* — H has an indexed digest and a certificate at H or at some later
//! height reachable through the contiguous digest index (a certificate vouches
//! for its whole recorded ancestry; this is the same covering authentication
//! marshal's backfill trusts). Coverage rather than per-height presence is
//! deliberate: certificates finalized while this validator was down never
//! re-broadcast, so a per-height rule would stall forever after any downtime,
//! while coverage heals at the first live certificate after catch-up. The
//! watermark only advances, and on a healthy chain it tracks the tip — a stall
//! is an honest end-to-end health signal (surfaced in `/status`).
//!
//! Everything keyed by *epoch* is additionally scoped by the consensus era
//! (the era digest's first 8 bytes): epoch numbering restarts per era, so a
//! disaster fork or re-migration would otherwise collide with the dead era's
//! records. The digest-keyed certificates are deliberately not scoped — they
//! are the permanent trail every era leaves behind — and the height→digest
//! index is dropped above the anchor when a different era is adopted (above a
//! fork's anchor, the new chain has no finalized heights yet).

use std::path::Path;
use std::sync::Mutex;
use tokio::sync::watch;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;
use zksync_os_wire::{EpochTransition, FinalityCertificate};

#[derive(Clone, Copy, Debug)]
pub enum FinalityCF {
    /// Certificate by the block's consensus digest (32 bytes).
    Certificates,
    /// Consensus digest by block height (u64, big-endian — RocksDB orders keys).
    HeightIndex,
    /// Small bookkeeping values; currently only the certified watermark.
    Meta,
    /// Epoch transition record by era-scoped epoch (era digest prefix ‖ epoch,
    /// big-endian) — the committee custody trail (see [`EpochTransition`]).
    /// Era-scoped because epoch numbering restarts per era: without the prefix,
    /// first-observed-wins would keep a dead era's records after a fork or
    /// re-migration.
    Transitions,
    /// Raw consensus-library-encoded finalizations by era-scoped round (era
    /// digest prefix ‖ epoch ‖ view, u64s big-endian), value = block digest
    /// (32 bytes) ‖ encoded finalization. Era-scoped so a dead era's cached
    /// floor can never hijack a fork's fresh start.
    /// A local cache, not a sovereign format: it exists so a restart with empty
    /// consensus storage can hand marshal a floor finalization (which the
    /// sovereign [`FinalityCertificate`] cannot reconstruct — it deliberately
    /// drops library-internal fields). A consensus-library upgrade may invalidate
    /// these bytes; readers must treat decode failure as "no floor" and fall
    /// back, never as an error. Pruned to the last two epochs.
    FloorCache,
    /// Registry derivation record by era-scoped epoch (era digest prefix ‖
    /// epoch, big-endian) — the on-chain registry's durable derivation trail
    /// (see
    /// [`zksync_os_wire::RegistryDerivation`]). Written first-observed-wins at
    /// each epoch's lookahead boundary; restarts and floor rebuilds replay it
    /// instead of re-deriving from (possibly pruned) historical state. Never
    /// pruned, like the custody trail it parallels.
    Derivations,
}

/// The floor-cache key for a round: era prefix, then epoch, then view (both
/// big-endian), so RocksDB's lexical key order is the round order within an
/// era (range-prunable, reverse-iterable).
fn round_key(era: [u8; 8], epoch: u64, view: u64) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[..8].copy_from_slice(&era);
    key[8..16].copy_from_slice(&epoch.to_be_bytes());
    key[16..].copy_from_slice(&view.to_be_bytes());
    key
}

/// An era-scoped epoch key (transitions, registry derivations).
fn epoch_key(era: [u8; 8], epoch: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&era);
    key[8..].copy_from_slice(&epoch.to_be_bytes());
    key
}

impl NamedColumnFamily for FinalityCF {
    const DB_NAME: &'static str = "finality";
    const ALL: &'static [Self] = &[
        FinalityCF::Certificates,
        FinalityCF::HeightIndex,
        FinalityCF::Meta,
        FinalityCF::Transitions,
        FinalityCF::FloorCache,
        FinalityCF::Derivations,
    ];

    fn name(&self) -> &'static str {
        match self {
            FinalityCF::Certificates => "certificates",
            FinalityCF::HeightIndex => "height_index",
            FinalityCF::Meta => "meta",
            FinalityCF::Transitions => "transitions",
            FinalityCF::FloorCache => "floor_cache",
            FinalityCF::Derivations => "registry_derivations",
        }
    }
}

const WATERMARK_KEY: &[u8] = b"certified_watermark";
const OBSERVED_ROUND_KEY: &[u8] = b"highest_observed_round";
const ERA_KEY: &[u8] = b"consensus_era";

pub struct FinalityStore {
    db: RocksDB<FinalityCF>,
    /// Serializes watermark advances (both writers try to advance after their
    /// write) and carries the covering probe's high-water cursor: the furthest
    /// contiguously-indexed height a probe has already scanned. The contiguous
    /// range only grows, so resuming there instead of at the watermark keeps a
    /// long-downtime hole from being fruitlessly rescanned on every live
    /// finalization during catch-up — exactly when the node is busiest.
    /// In-memory only: after a restart the first probe rescans once and
    /// re-establishes it.
    /// and a read-modify-write race would otherwise lose ground (it would self-heal
    /// on the next write, but there is no reason to allow the gap).
    watermark_lock: Mutex<u64>,
    /// Broadcasts watermark advances to the status surface.
    watermark_watch: watch::Sender<Option<u64>>,
    /// The recorded consensus era's digest prefix, scoping every epoch-keyed
    /// entry (see the module docs on eras). Zero before an era is recorded —
    /// the legacy shape; node startup records the era before consensus runs.
    era: Mutex<[u8; 8]>,
}

impl FinalityStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = RocksDB::new(path)?;
        let store = Self {
            db,
            watermark_lock: Mutex::new(0),
            watermark_watch: watch::channel(None).0,
            era: Mutex::new([0u8; 8]),
        };
        if let Some(era) = store.consensus_era()? {
            *store.era.lock().unwrap() = era[..8].try_into().expect("8 of 32 bytes");
        }
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

    /// The current era's key prefix. Epoch numbering restarts per era
    /// (a fork or re-migration restarts epochs at zero), so every epoch-keyed
    /// entry is scoped by it — otherwise first-observed-wins would keep a dead
    /// era's records and a dead era's cached floor could hijack a fork start.
    fn era_prefix(&self) -> [u8; 8] {
        *self.era.lock().unwrap()
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
        // A new certificate is what can lift the watermark over an uncovered
        // range, so this is the one place the covering probe runs.
        self.advance_watermark(true)
    }

    /// Records which digest was finalized at `height`. Idempotent; called by the
    /// execution environment for every delivered finalized block.
    pub fn index_height(&self, height: u64, digest: [u8; 32]) -> anyhow::Result<()> {
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::HeightIndex, &height.to_be_bytes(), &digest);
        self.db.write(batch)?;
        self.advance_watermark(false)
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
        // Serialize the persistent transition with watermark writers. The era
        // marker, the height-index cut, and the watermark are one recovery
        // decision: after a crash RocksDB must expose either all of the old era
        // or all of the new one, never a mixture of the two.
        let mut probe_cursor = self.watermark_lock.lock().unwrap();
        let era_changed = self
            .consensus_era()?
            .is_some_and(|recorded| recorded != genesis_digest);
        let current = self.certified_watermark()?.unwrap_or(0);
        let era_anchor_watermark = (anchor_height > 0).then_some(anchor_height);
        let floor_to_anchor = !era_changed && anchor_height > 0 && current < anchor_height;

        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::Meta, ERA_KEY, &genesis_digest);
        if era_changed {
            // The height→digest index answers "what is finalized at height h
            // on this chain" — above a fork's anchor, nothing is, yet. Drop
            // the dead era's entries in the same batch that records the new
            // era and its exact watermark. Certificates are digest-keyed and
            // remain forever as the immutable audit trail.
            batch.delete_range_cf(
                FinalityCF::HeightIndex,
                (&(anchor_height + 1).to_be_bytes()[..])..(&u64::MAX.to_be_bytes()[..]),
            );
            if let Some(watermark) = era_anchor_watermark {
                batch.put_cf(FinalityCF::Meta, WATERMARK_KEY, &watermark.to_be_bytes());
            } else {
                batch.delete_cf(FinalityCF::Meta, WATERMARK_KEY);
            }
        } else if floor_to_anchor {
            // First recording or the same era: the anchor is a monotonic floor,
            // never a reason to discard already certified progress.
            batch.put_cf(
                FinalityCF::Meta,
                WATERMARK_KEY,
                &anchor_height.to_be_bytes(),
            );
        }
        self.db.write(batch)?;

        // Publish the new in-memory view only after the complete persistent
        // transition succeeded.
        *self.era.lock().unwrap() = genesis_digest[..8].try_into().expect("8 of 32 bytes");
        if era_changed {
            *probe_cursor = anchor_height;
            self.watermark_watch.send_replace(era_anchor_watermark);
        } else if floor_to_anchor {
            self.watermark_watch.send_replace(Some(anchor_height));
        }
        drop(probe_cursor);

        // Certificates observed before the era was recorded may already continue
        // past the floor — including covering ones above a gap.
        self.advance_watermark(true)
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
        let key = [OBSERVED_ROUND_KEY, &self.era_prefix()[..]].concat();
        batch.put_cf(FinalityCF::Meta, &key, &value);
        self.db.write(batch)?;
        Ok(())
    }

    /// The highest `(epoch, view)` ever observed by this validator, if any.
    pub fn highest_observed_round(&self) -> anyhow::Result<Option<(u64, u64)>> {
        let key = [OBSERVED_ROUND_KEY, &self.era_prefix()[..]].concat();
        let Some(bytes) = self.db.get_cf(FinalityCF::Meta, &key)? else {
            return Ok(None);
        };
        anyhow::ensure!(bytes.len() == 16, "stored observed round is not two u64s");
        let epoch = u64::from_be_bytes(bytes[..8].try_into().expect("checked length"));
        let view = u64::from_be_bytes(bytes[8..].try_into().expect("checked length"));
        Ok(Some((epoch, view)))
    }

    /// Records the committee custody trail entry for an epoch, at the *first
    /// observed* finalization of that epoch. First-observed wins: replays and
    /// backfills that re-report the epoch leave the original record untouched — an
    /// audit trail that could be rewritten would not be one. Returns whether the
    /// record was written (false: one already existed).
    pub fn record_epoch_transition(&self, transition: &EpochTransition) -> anyhow::Result<bool> {
        use commonware_codec::{EncodeSize, Write as _};
        // The lock doubles as the writer serializer for check-then-put (both
        // observer threads route through the same store instance).
        let _guard = self.watermark_lock.lock().unwrap();
        let key = epoch_key(self.era_prefix(), transition.epoch);
        if self.db.get_cf(FinalityCF::Transitions, &key)?.is_some() {
            return Ok(false);
        }
        let mut encoded = Vec::with_capacity(transition.encode_size());
        transition.write(&mut encoded);
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::Transitions, &key, &encoded);
        self.db.write(batch)?;
        Ok(true)
    }

    /// The custody trail entry for `epoch`, if consensus has been observed entering
    /// it on this node.
    pub fn epoch_transition(&self, epoch: u64) -> anyhow::Result<Option<EpochTransition>> {
        use commonware_codec::Read as _;
        let Some(bytes) = self.db.get_cf(
            FinalityCF::Transitions,
            &epoch_key(self.era_prefix(), epoch),
        )?
        else {
            return Ok(None);
        };
        let transition = EpochTransition::read_cfg(&mut bytes.as_slice(), &())
            .map_err(|err| anyhow::anyhow!("stored epoch transition does not decode: {err}"))?;
        Ok(Some(transition))
    }

    /// Records one registry derivation, at the epoch's lookahead boundary.
    /// First-observed wins, exactly like the custody trail: the outcome at a
    /// fixed height is a chain fact, and re-derivations (restarts, replays)
    /// leave the original untouched. Returns whether the record was written.
    pub fn record_registry_derivation(
        &self,
        derivation: &zksync_os_wire::RegistryDerivation,
    ) -> anyhow::Result<bool> {
        use commonware_codec::{EncodeSize, Write as _};
        // The lock doubles as the writer serializer for check-then-put.
        let _guard = self.watermark_lock.lock().unwrap();
        let key = epoch_key(self.era_prefix(), derivation.epoch);
        if self.db.get_cf(FinalityCF::Derivations, &key)?.is_some() {
            return Ok(false);
        }
        let mut encoded = Vec::with_capacity(derivation.encode_size());
        derivation.write(&mut encoded);
        let mut batch = self.db.new_write_batch();
        batch.put_cf(FinalityCF::Derivations, &key, &encoded);
        self.db.write(batch)?;
        Ok(true)
    }

    /// Every recorded registry derivation, ascending by epoch — replayed into
    /// the committee source at startup so recorded epochs are never re-derived.
    pub fn registry_derivations(&self) -> anyhow::Result<Vec<zksync_os_wire::RegistryDerivation>> {
        use commonware_codec::Read as _;
        let era = self.era_prefix();
        self.db
            .from_iterator_cf(FinalityCF::Derivations, &era[..]..)
            .take_while(|(key, _)| key.starts_with(&era))
            .map(|(key, bytes)| {
                anyhow::ensure!(key.len() == 16, "derivation key is not era-scoped");
                zksync_os_wire::RegistryDerivation::read_cfg(&mut bytes.as_ref(), &()).map_err(
                    |err| anyhow::anyhow!("stored registry derivation does not decode: {err}"),
                )
            })
            .collect()
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

    /// Caches a consensus-library-encoded finalization for floor-started restarts
    /// (see [`FinalityCF::FloorCache`] for the cache-not-format contract).
    /// Idempotent; called by the consensus activity observer at every finalization.
    pub fn put_raw_finalization(
        &self,
        epoch: u64,
        view: u64,
        digest: [u8; 32],
        raw: &[u8],
    ) -> anyhow::Result<()> {
        let mut value = Vec::with_capacity(32 + raw.len());
        value.extend_from_slice(&digest);
        value.extend_from_slice(raw);
        let mut batch = self.db.new_write_batch();
        batch.put_cf(
            FinalityCF::FloorCache,
            &round_key(self.era_prefix(), epoch, view),
            &value,
        );
        self.db.write(batch)?;
        Ok(())
    }

    /// Drops cached raw finalizations below `epoch` — called when an epoch
    /// transition is first observed, keeping the cache to roughly two epochs
    /// (a floor older than that fails the freshness policy anyway).
    pub fn prune_raw_finalizations_below(&self, epoch: u64) -> anyhow::Result<()> {
        let era = self.era_prefix();
        let mut batch = self.db.new_write_batch();
        batch.delete_range_cf(
            FinalityCF::FloorCache,
            (&round_key(era, 0, 0)[..])..(&round_key(era, epoch, 0)[..]),
        );
        self.db.write(batch)?;
        Ok(())
    }

    /// Cached raw finalizations, newest round first: `(epoch, view, block digest,
    /// consensus-library-encoded finalization)`. `limit` bounds the scan; the
    /// caller matches digests against its chain to pick a floor at or below its
    /// committed tip.
    pub fn raw_finalizations_newest_first(
        &self,
        limit: usize,
    ) -> Vec<(u64, u64, [u8; 32], Vec<u8>)> {
        let era = self.era_prefix();
        self.db
            .to_iterator_cf(
                FinalityCF::FloorCache,
                ..=&round_key(era, u64::MAX, u64::MAX)[..],
            )
            .take_while(|(key, _)| key.starts_with(&era))
            .take(limit)
            .filter_map(|(key, value)| {
                if key.len() != 24 || value.len() < 32 {
                    return None;
                }
                let epoch = u64::from_be_bytes(key[8..16].try_into().expect("checked length"));
                let view = u64::from_be_bytes(key[16..].try_into().expect("checked length"));
                let digest: [u8; 32] = value[..32].try_into().expect("checked length");
                Some((epoch, view, digest, value[32..].to_vec()))
            })
            .collect()
    }

    /// The newest epoch with a recorded transition (committee custody entry) —
    /// the reference point for the floor freshness policy.
    pub fn latest_transition_epoch(&self) -> Option<u64> {
        let era = self.era_prefix();
        self.db
            .to_iterator_cf(FinalityCF::Transitions, ..=&epoch_key(era, u64::MAX)[..])
            .next()
            .filter(|(key, _)| key.starts_with(&era))
            .and_then(|(key, _)| key[8..].try_into().ok().map(u64::from_be_bytes))
    }

    /// The digest finalized at `height`, if the commit path has indexed it.
    pub fn digest_at_height(&self, height: u64) -> anyhow::Result<Option<[u8; 32]>> {
        let Some(digest) = self
            .db
            .get_cf(FinalityCF::HeightIndex, &height.to_be_bytes())?
        else {
            return Ok(None);
        };
        digest
            .try_into()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("stored height index is not a 32-byte digest"))
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

    /// Advances the certified watermark. A height is *covered* the way the
    /// protocol itself authenticates history during catch-up: by a certificate
    /// at that height or by any later certificate reachable through the
    /// contiguous digest index (each digest commits to its parent, so one
    /// certificate vouches for its whole recorded ancestry). Certificates for
    /// heights finalized while this validator was down never re-broadcast —
    /// marshal's backfill deliberately fetches blocks, not per-height
    /// certificates — so a per-height rule would stall on such a hole forever;
    /// the covering rule heals at the first live certificate after catch-up.
    ///
    /// Two motions, both monotone:
    /// - the dense walk (every advance): step while the next height has its own
    ///   certificate — amortized O(1), each height walked once;
    /// - the covering probe (`certificate_arrived` only): from the stall point,
    ///   scan the contiguous digest range ahead for the highest height with a
    ///   certificate and jump there. Probes only run when a certificate landed,
    ///   so backfill's index-only churn never rescans the hole.
    fn advance_watermark(&self, certificate_arrived: bool) -> anyhow::Result<()> {
        let mut probe_cursor = self.watermark_lock.lock().unwrap();
        let started_at = self.certified_watermark()?;
        let mut watermark = started_at;

        loop {
            // Dense walk: consecutive heights carrying their own certificates.
            while self
                .certificate_by_height(watermark.unwrap_or(0) + 1)?
                .is_some()
            {
                watermark = Some(watermark.unwrap_or(0) + 1);
            }
            if !certificate_arrived {
                break;
            }
            // Covering probe: the highest certified height reachable through
            // contiguous digests above the stall point. Resumes from the
            // cached cursor — heights scanned by an earlier probe are never
            // rescanned, so during a long catch-up each live finalization
            // pays for the newly indexed range only, not the whole hole.
            // (The trade: a certificate arriving late for an already-scanned
            // interior height is not noticed until a newer certificate lands;
            // on a live chain one always does, and on a halted chain the
            // watermark is not the alarm that matters.)
            let mut cursor = (*probe_cursor).max(watermark.unwrap_or(0));
            let mut jump_to = None;
            while let Some(digest) = self.digest_at_height(cursor + 1)? {
                cursor += 1;
                if self.certificate_by_digest(&digest)?.is_some() {
                    jump_to = Some(cursor);
                }
            }
            *probe_cursor = cursor;
            match jump_to {
                Some(covered) if Some(covered) > watermark => {
                    tracing::info!(
                        from = watermark.unwrap_or(0),
                        to = covered,
                        "certified watermark jumped an uncovered range (certificates \
                         finalized during downtime are covered by a later one)",
                    );
                    watermark = Some(covered);
                    // The jump may expose a dense run right above it.
                    continue;
                }
                _ => break,
            }
        }

        if watermark != started_at {
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

    /// The downtime shape: heights finalized while a validator is down are
    /// backfilled as blocks only — their per-height certificates never
    /// re-broadcast. The first live certificate after catch-up must cover the
    /// hole (a certificate vouches for its recorded ancestry), or the
    /// watermark stalls forever, which is exactly what soaks observed under
    /// the per-height rule.
    #[test]
    fn a_later_certificate_covers_a_downtime_hole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");

        // Live before the crash: heights 1..=3, densely certified.
        for height in 1..=3u64 {
            store
                .index_height(height, [height as u8; 32])
                .expect("write");
            store
                .put_certificate(&certificate(height, [height as u8; 32]))
                .expect("write");
        }
        assert_eq!(store.certified_watermark().expect("read"), Some(3));

        // Down for heights 4..=6: backfill re-commits the blocks (index only).
        for height in 4..=6u64 {
            store
                .index_height(height, [height as u8; 32])
                .expect("write");
        }
        assert_eq!(
            store.certified_watermark().expect("read"),
            Some(3),
            "index-only backfill must not move the watermark",
        );

        // Live again: height 7 lands with its certificate — covering 4..=6.
        store.index_height(7, [7; 32]).expect("write");
        store
            .put_certificate(&certificate(7, [7; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(7));
    }

    /// The covering jump never crosses a digest gap (nothing chains across
    /// it), and a re-observed certificate — the scout re-hears them — is
    /// enough to trigger the probe once the gap closes.
    #[test]
    fn covering_stops_at_digest_gaps_until_they_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");

        store.index_height(1, [1; 32]).expect("write");
        store
            .put_certificate(&certificate(1, [1; 32]))
            .expect("write");
        // Digests 2..=3 present, 4 missing, 5 present and certified.
        store.index_height(2, [2; 32]).expect("write");
        store.index_height(3, [3; 32]).expect("write");
        store.index_height(5, [5; 32]).expect("write");
        store
            .put_certificate(&certificate(5, [5; 32]))
            .expect("write");
        assert_eq!(
            store.certified_watermark().expect("read"),
            Some(1),
            "a certificate beyond a digest gap covers nothing",
        );

        // The gap closes (backfill delivers height 4); the certificate for 5
        // is re-observed — put_certificate is idempotent — and now covers.
        store.index_height(4, [4; 32]).expect("write");
        store
            .put_certificate(&certificate(5, [5; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(5));
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
    fn same_era_re_recording_only_floors_the_watermark_upward() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");
        store.record_consensus_era([7; 32], 20).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(20));

        // A same-era re-record with a *higher* anchor (a deeper pre-consensus
        // import) floors up to it: certificates for the gap will never exist.
        store.record_consensus_era([7; 32], 25).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(25));

        // With the watermark already at or above the anchor, re-recording must
        // not floor *down* — that would un-certify certified history.
        store.record_consensus_era([7; 32], 24).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(25));
    }

    #[test]
    fn a_new_era_over_an_anchor_floors_the_watermark_and_cuts_the_index_above_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");
        store.record_consensus_era([0xA; 32], 0).expect("era A");
        for height in 1..=3u64 {
            store
                .index_height(height, [height as u8; 32])
                .expect("write");
            store
                .put_certificate(&certificate(height, [height as u8; 32]))
                .expect("write");
        }
        assert_eq!(store.certified_watermark().expect("read"), Some(3));

        // The disaster-hardfork shape: truncate to 2, fork into a new era
        // anchored there. Heights above the anchor belong to the dead era and
        // leave the index; the anchor itself is shared history and stays.
        store.record_consensus_era([0xB; 32], 2).expect("era B");
        assert_eq!(store.certified_watermark().expect("read"), Some(2));
        assert!(store.digest_at_height(2).expect("read").is_some());
        assert!(store.digest_at_height(3).expect("read").is_none());

        // An era anchored *above* every certificate (a deeper pre-consensus
        // import): the anchor floor is load-bearing here — no walk over the
        // certificate index can reach 5 on its own.
        store.record_consensus_era([0xC; 32], 5).expect("era C");
        assert_eq!(store.certified_watermark().expect("read"), Some(5));
    }

    #[test]
    fn a_new_era_at_genesis_clears_the_watermark() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");
        store.record_consensus_era([0xA; 32], 0).expect("era A");
        store.index_height(1, [1; 32]).expect("write");
        store
            .put_certificate(&certificate(1, [1; 32]))
            .expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(1));
        let watch = store.watermark_subscription();

        store.record_consensus_era([0xB; 32], 0).expect("era B");

        assert_eq!(store.certified_watermark().expect("read"), None);
        assert_eq!(*watch.borrow(), None);
        assert!(store.digest_at_height(1).expect("read").is_none());

        drop(store);
        let store = FinalityStore::open(dir.path()).expect("reopen");
        assert_eq!(store.consensus_era().expect("read"), Some([0xB; 32]));
        assert_eq!(store.certified_watermark().expect("read"), None);
        assert_eq!(*store.watermark_subscription().borrow(), None);
        assert!(store.digest_at_height(1).expect("read").is_none());
        assert!(
            store
                .certificate_by_digest(&[1; 32])
                .expect("read")
                .is_some(),
            "era adoption never deletes the immutable certificate trail"
        );
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
    #[test]
    fn epoch_transitions_are_first_observed_wins_and_survive_reopen() {
        use zksync_os_wire::{CommitteeMemberKeys, EpochTransition};

        let transition = |epoch: u64, digest: [u8; 32]| EpochTransition {
            epoch,
            scheme: SignatureScheme::Bls12381Multisig,
            committee: vec![
                CommitteeMemberKeys {
                    network_key: [1; 32],
                    bls_key: [2; 48],
                },
                CommitteeMemberKeys {
                    network_key: [3; 32],
                    bls_key: [4; 48],
                },
            ],
            first_finalized_digest: digest,
            first_finalized_view: 1,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = FinalityStore::open(dir.path()).expect("open");
            assert!(store.epoch_transition(3).expect("read").is_none());
            assert!(
                store
                    .record_epoch_transition(&transition(3, [0xAA; 32]))
                    .expect("write"),
                "first record for the epoch is written"
            );
            // A replayed observation with different content must not rewrite the
            // custody trail.
            assert!(
                !store
                    .record_epoch_transition(&transition(3, [0xBB; 32]))
                    .expect("write"),
                "re-observation leaves the original untouched"
            );
        }

        let store = FinalityStore::open(dir.path()).expect("reopen");
        let stored = store.epoch_transition(3).expect("read").expect("present");
        assert_eq!(stored.first_finalized_digest, [0xAA; 32]);
        assert_eq!(stored.committee.len(), 2);
        assert!(store.epoch_transition(4).expect("read").is_none());
    }

    /// The fork/re-migration shape (plan F1): a new era restarts epochs at
    /// zero, so every epoch-keyed trail must be invisible across eras. The
    /// height index above the anchor is discarded and the watermark resets,
    /// while digest-keyed certificates remain as the immutable audit trail.
    #[test]
    fn a_new_era_scopes_the_epoch_keyed_trails_and_resets_the_watermark() {
        use zksync_os_wire::{CommitteeMemberKeys, EpochTransition};

        let transition = |epoch: u64, digest: [u8; 32]| EpochTransition {
            epoch,
            scheme: SignatureScheme::Bls12381Multisig,
            committee: vec![CommitteeMemberKeys {
                network_key: [1; 32],
                bls_key: [2; 48],
            }],
            first_finalized_digest: digest,
            first_finalized_view: 1,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let store = FinalityStore::open(dir.path()).expect("open");

        // Era A lives: certificates, custody, floor cache, observed rounds.
        store.record_consensus_era([0xA; 32], 0).expect("era A");
        for height in 1..=5u64 {
            store
                .index_height(height, [height as u8; 32])
                .expect("write");
            store
                .put_certificate(&certificate(height, [height as u8; 32]))
                .expect("write");
        }
        assert!(
            store
                .record_epoch_transition(&transition(0, [0xA1; 32]))
                .expect("write")
        );
        store
            .put_raw_finalization(0, 7, [0xA2; 32], b"era-a-floor")
            .expect("write");
        store.note_observed_round(0, 40).expect("write");
        assert_eq!(store.certified_watermark().expect("read"), Some(5));

        // The fork: a different era anchored at 3. Epoch-keyed trails must
        // read empty; the certificate trail must not.
        store.record_consensus_era([0xB; 32], 3).expect("era B");
        drop(store);
        let store = FinalityStore::open(dir.path()).expect("reopen after era adoption");
        assert_eq!(store.consensus_era().expect("read"), Some([0xB; 32]));
        assert_eq!(
            store.certified_watermark().expect("read"),
            Some(3),
            "coverage above the anchor belonged to the dead era"
        );
        assert!(
            store.epoch_transition(0).expect("read").is_none(),
            "era A's epoch-0 custody record must not answer for era B"
        );
        assert!(store.latest_transition_epoch().is_none());
        assert!(store.raw_finalizations_newest_first(10).is_empty());
        assert_eq!(
            store.highest_observed_round().expect("read"),
            None,
            "era A's observed-round floor must not gate era B"
        );
        assert!(
            store.certificate_by_height(5).expect("read").is_none(),
            "above the fork anchor, this chain has no finalized heights yet"
        );
        assert!(
            store
                .certificate_by_digest(&[5; 32])
                .expect("read")
                .is_some(),
            "the abandoned era's certificates remain, forever, by digest"
        );

        // Era B's own trails land under fresh keys — first-observed-wins is
        // now per era.
        assert!(
            store
                .record_epoch_transition(&transition(0, [0xB1; 32]))
                .expect("write")
        );
        assert_eq!(
            store
                .epoch_transition(0)
                .expect("read")
                .expect("present")
                .first_finalized_digest,
            [0xB1; 32]
        );
        store.note_observed_round(0, 2).expect("write");
        assert_eq!(store.highest_observed_round().expect("read"), Some((0, 2)));

        // Reopening lands back in era B, not the zero era.
        drop(store);
        let store = FinalityStore::open(dir.path()).expect("reopen");
        assert_eq!(store.latest_transition_epoch(), Some(0));
        assert_eq!(
            store
                .epoch_transition(0)
                .expect("read")
                .expect("present")
                .first_finalized_digest,
            [0xB1; 32]
        );
    }

    #[test]
    fn floor_cache_orders_prunes_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = FinalityStore::open(dir.path()).expect("open");
            // Rounds across two epochs, written out of order — retrieval must be
            // newest-round-first regardless of write order.
            store
                .put_raw_finalization(1, 7, [1; 32], b"one-seven")
                .expect("write");
            store
                .put_raw_finalization(2, 1, [2; 32], b"two-one")
                .expect("write");
            store
                .put_raw_finalization(1, 9, [3; 32], b"one-nine")
                .expect("write");
            let newest_first = store.raw_finalizations_newest_first(10);
            let rounds: Vec<(u64, u64)> = newest_first
                .iter()
                .map(|(epoch, view, _, _)| (*epoch, *view))
                .collect();
            assert_eq!(rounds, vec![(2, 1), (1, 9), (1, 7)]);
            assert_eq!(newest_first[0].2, [2; 32]);
            assert_eq!(newest_first[0].3, b"two-one");

            // Pruning below epoch 2 drops both epoch-1 entries.
            store.prune_raw_finalizations_below(2).expect("prune");
            let remaining = store.raw_finalizations_newest_first(10);
            assert_eq!(remaining.len(), 1);
            assert_eq!((remaining[0].0, remaining[0].1), (2, 1));
        }

        // Cache entries are durable across reopen (a restart is exactly when the
        // floor is needed).
        let store = FinalityStore::open(dir.path()).expect("reopen");
        let remaining = store.raw_finalizations_newest_first(10);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].3, b"two-one");
    }
}
