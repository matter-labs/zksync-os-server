//! Startup lifecycle guards: consensus eras, rollback and truncation
//! acknowledgments, the storage instance lock, and where a restarting stack
//! resumes from.

use anyhow::Context as _;
use std::path::PathBuf;
use zksync_os_consensus_core::types::SchemeProvider;

/// What startup should do about the consensus era: proceed on a match, or record
/// the (new) era. Any state that could mix two consensus histories is an error.
#[derive(Debug, PartialEq, Eq)]
pub enum EraDecision {
    /// The recorded era matches the configured anchor — normal operation.
    Proceed,
    /// Record the configured era: the first consensus start of this chain, a
    /// deliberate re-migration over cleared engine state, or an instance from
    /// before era tracking existed.
    Adopt,
}

/// The consensus-era guard, pure so the whole matrix is unit-testable. The era is
/// the consensus genesis digest (anchor height + anchored block hash): recorded at
/// the first consensus start, compared on every later one.
pub fn decide_consensus_era(
    recorded: Option<[u8; 32]>,
    configured: [u8; 32],
    engine_state_is_fresh: bool,
    wal_tip: u64,
    anchor_height: u64,
    // The operator's `consensus.acknowledge_fork`, parsed: the anchor height
    // and block hash being deliberately forked/re-migrated to.
    acknowledged_fork: Option<(u64, alloy::primitives::B256)>,
    // The hash this node's own chain has at the anchor height — what the
    // acknowledgment must name (catches a truncation that landed on the wrong
    // chain *before* this node quietly forms its own lonely era).
    local_hash_at_anchor: alloy::primitives::B256,
) -> anyhow::Result<EraDecision> {
    match (recorded, engine_state_is_fresh) {
        (Some(era), _) if era == configured => Ok(EraDecision::Proceed),
        (Some(_), false) => anyhow::bail!(
            "this chain previously ran consensus with a different anchor than \
             `consensus.genesis_height` = {anchor_height} derives. If this is a deliberate \
             re-migration after a rollback, clear the consensus engine state and restart; \
             otherwise fix the configured genesis height"
        ),
        // A different era over deliberately cleared engine state: a disaster
        // fork or a re-migration. Either way finalized history is being
        // overridden, so the operator must acknowledge exactly what they are
        // starting into — the anchor height and its hash — and the chain must
        // end exactly there.
        (Some(_), true) => {
            let (acknowledged_height, acknowledged_hash) = acknowledged_fork.context(
                "this chain previously ran consensus under a different era; starting into \
                 a new anchor abandons finalized history and requires \
                 `consensus.acknowledge_fork = \"<height>:<block hash at height>\"` \
                 naming the new anchor",
            )?;
            anyhow::ensure!(
                acknowledged_height == anchor_height,
                "`consensus.acknowledge_fork` names height {acknowledged_height} but \
                 `consensus.genesis_height` is {anchor_height} — the acknowledgment must \
                 name exactly the anchor being started into"
            );
            anyhow::ensure!(
                acknowledged_hash == local_hash_at_anchor,
                "`consensus.acknowledge_fork` names hash {acknowledged_hash} at height \
                 {anchor_height}, but this node's chain has {local_hash_at_anchor} there — \
                 the truncation on this node did not land on the agreed block; do not \
                 start it (re-check the truncation and the agreed anchor)"
            );
            anyhow::ensure!(
                wal_tip == anchor_height,
                "a consensus era must start exactly at the agreed cutover: the write-ahead \
                 log ends at {wal_tip} but `consensus.genesis_height` is {anchor_height}"
            );
            Ok(EraDecision::Adopt)
        }
        // No era at all over fresh state: the first consensus start of this
        // chain (fresh chain or first migration) — nothing finalized is being
        // overridden, no acknowledgment needed; the cutover must still be exact.
        (None, true) => {
            anyhow::ensure!(
                wal_tip == anchor_height,
                "a consensus era must start exactly at the agreed cutover: the write-ahead \
                 log ends at {wal_tip} but `consensus.genesis_height` is {anchor_height}"
            );
            Ok(EraDecision::Adopt)
        }
        // No marker over existing engine state: an instance from before era tracking
        // existed. Adopt its era (the anchor still derives it — a mismatch would have
        // broken consensus itself long before this check).
        (None, false) => Ok(EraDecision::Adopt),
    }
}

/// Parses `consensus.acknowledge_fork`: `"<height>:<block hash at height>"`.
pub fn parse_acknowledge_fork(
    value: &Option<String>,
) -> anyhow::Result<Option<(u64, alloy::primitives::B256)>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (height, hash) = value
        .split_once(':')
        .context("`consensus.acknowledge_fork` must be `\"<height>:<block hash at height>\"`")?;
    let height: u64 = height
        .trim()
        .parse()
        .context("`consensus.acknowledge_fork` height is not a number")?;
    let hash: alloy::primitives::B256 = hash
        .trim()
        .parse()
        .context("`consensus.acknowledge_fork` hash is not a 32-byte hex hash")?;
    Ok(Some((height, hash)))
}

/// The rollback guard: single-sequencer operation over existing consensus state
/// strands that state and a later re-enable could mix histories — refuse unless the
/// operator acknowledged the rollback. Never deletes anything.
pub fn check_rollback_acknowledged(
    has_consensus_state: bool,
    acknowledged: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !has_consensus_state || acknowledged,
        "this chain has consensus state but consensus is disabled. If this rollback to \
         single-sequencer operation is deliberate, set `consensus.acknowledge_rollback: \
         true`; the consensus state is left untouched"
    );
    Ok(())
}

/// Where this validator's consensus state starts (see
/// [`zksync_os_consensus_core::StackStart`]): a cached finality floor when one is
/// usable — bounding an empty-storage start's catch-up to the blocks above it —
/// or the era genesis (full backfill). With existing consensus storage marshal
/// ignores a floor at or below what it already processed, so this selection only
/// changes the empty-storage cases: a rebuild after an incident, or a node
/// promoted into the committee with a retained chain.
pub(super) fn select_stack_start(
    context: &mut commonware_runtime::tokio::Context,
    finality: &zksync_os_consensus_execution::FinalityStore,
    provider: &SchemeProvider,
    era_anchor: u64,
    chain_tip: u64,
    accept_stale_floor: bool,
) -> zksync_os_consensus_core::StackStart<commonware_cryptography::sha256::Digest> {
    use commonware_codec::Read as _;
    use commonware_cryptography::certificate::Scheme as _;
    use zksync_os_consensus_core::StackStart;

    // A floor must anchor at or below the chain tip (marshal re-delivers the floor
    // block, then everything above it; a floor above the tip would leave a delivery
    // gap the chain can never fill) and above the era anchor (the anchor itself is
    // the Genesis start). The window bounds startup work; finalizations are dense,
    // so the newest cached one is normally within a few heights of the tip.
    const WINDOW: u64 = 1024;
    const RAW_SCAN: usize = 4096;
    let low = chain_tip.saturating_sub(WINDOW).max(era_anchor + 1);
    if chain_tip < low {
        return StackStart::Genesis;
    }
    let mut heights_by_digest = std::collections::HashMap::new();
    for height in low..=chain_tip {
        if let Ok(Some(digest)) = finality.digest_at_height(height) {
            heights_by_digest.insert(digest, height);
        }
    }

    let latest_transition = finality.latest_transition_epoch();
    for (epoch, view, digest, raw) in finality.raw_finalizations_newest_first(RAW_SCAN) {
        let Some(height) = heights_by_digest.get(&digest) else {
            continue;
        };
        // Freshness policy (ratified in the EN-convergence design): a floor from
        // before the committee's last scheduled change is refused — the full
        // backfill re-derives everything instead. Entries are scanned newest
        // first, so every later candidate is staler: stop here.
        if let Some(latest) = latest_transition
            && epoch < latest
            && !accept_stale_floor
        {
            tracing::warn!(
                floor_epoch = epoch,
                latest_transition_epoch = latest,
                "cached finality floor predates the last committee change; \
                 falling back to a full backfill (set `consensus.accept_stale_floor` \
                 to use it anyway)"
            );
            return StackStart::Genesis;
        }
        // Cache semantics: an entry that no longer decodes or verifies is skipped,
        // not fatal — entries fail independently. A consensus-library upgrade
        // invalidates all of them (the scan falls through to Genesis); a corrected
        // committee schedule invalidates only the entries a misconfigured node
        // "verified" under its stale scheme (a stalled validator's cache really
        // does hold stale-width certificates for the epoch it stalled in), and an
        // older, genuinely-valid floor behind them is still worth finding.
        let scheme = provider.scheme_for(zksync_os_consensus_core::types::Epoch::new(epoch));
        let Ok(finalization) =
            zksync_os_consensus_core::types::Finalization::<
                zksync_os_consensus_core::types::Scheme,
                commonware_cryptography::sha256::Digest,
            >::read_cfg(&mut raw.as_slice(), &scheme.certificate_codec_config())
        else {
            tracing::warn!(
                epoch,
                view,
                "skipping a cached finality floor that no longer decodes (library \
                 upgrade, or a certificate recorded under a corrected-away schedule)"
            );
            continue;
        };
        if finalization.proposal.payload.as_ref() != digest
            || !finalization.verify(
                context,
                scheme.as_ref(),
                &zksync_os_consensus_core::types::Sequential,
            )
        {
            tracing::warn!(
                epoch,
                view,
                "skipping a cached finality floor that does not verify under the \
                 configured schedule"
            );
            continue;
        }
        tracing::info!(
            height,
            epoch,
            view,
            "consensus will start from a cached finality floor if its storage is empty"
        );
        return StackStart::Floor(Box::new(finalization));
    }
    StackStart::Genesis
}

/// Path of the advisory lock that serializes consensus instances on one storage
/// directory. Also useful to *observe*: whoever can take this lock knows no consensus
/// instance (with everything it holds) is alive on this storage.
pub fn instance_lock_path(storage_directory: &std::path::Path) -> PathBuf {
    storage_directory.join(".instance-lock")
}

/// Marker the truncate tool plants inside a consensus engine state directory
/// whose recorded progress extends past a chain truncation: marshal would
/// resume delivery above the truncated tip and die on the delivery-order
/// assert, so startup refuses to run consensus over a flagged directory.
/// The runbook's "clear the consensus engine state" step removes the flag
/// together with the state it poisons.
pub fn truncation_flag_path(storage_directory: &std::path::Path) -> PathBuf {
    storage_directory.join(".predates-truncation")
}

/// The truncation point recorded in [`truncation_flag_path`]'s marker, when
/// one is present.
pub fn read_truncation_flag(
    storage_directory: &std::path::Path,
) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(truncation_flag_path(storage_directory)) {
        Ok(contents) => Ok(Some(contents.trim().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}
