//! Observability surfaces fed from inside the consensus world: activity
//! reporting into the finality store, and the status/registry views.

use zksync_os_consensus_core::CommitteeSource;
use zksync_os_consensus_core::types::{Activity, Attributable as _, ConsensusActivity};
use zksync_os_consensus_execution::metrics::CONSENSUS_METRICS;
use zksync_os_status_server::{ConsensusMetricsEncoder, FinalizedObservation, RegistryStatus};

use crate::config::RegistryMode;

/// Handles through which the consensus world reports its progress to the node's
/// status/metrics surfaces. All senders; the receivers live in the status server.
pub struct ConsensusObservability {
    /// The latest finalized round this validator observed.
    pub finalized: tokio::sync::watch::Sender<Option<FinalizedObservation>>,
    /// Installed once the consensus runtime is up: encodes its prometheus registry
    /// (engine, marshal, p2p actors) on demand.
    pub metrics_encoder: tokio::sync::watch::Sender<Option<ConsensusMetricsEncoder>>,
    /// The node's sovereign finality store: every observed finalization certificate
    /// is converted to the node's own format and persisted here.
    pub finality: std::sync::Arc<zksync_os_consensus_execution::FinalityStore>,
    /// The latest registry derivation (shadow/config_shadow modes; stays `None` in
    /// `schedule` mode and on nodes without a registry).
    pub registry: tokio::sync::watch::Sender<Option<RegistryStatus>>,
}

/// The status-surface form of one derivation observation. The committee hash is
/// the cross-node comparison handle (like the chain fingerprint: the first 8
/// bytes of a canonical sha256, hex) — two nodes disagreeing on it for the same
/// epoch is registry drift even when both individually report `matches_config`.
pub(super) fn registry_status(
    mode: RegistryMode,
    observation: &zksync_os_consensus_core::RegistryObservation,
) -> RegistryStatus {
    use commonware_codec::Encode as _;
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for (network_key, bls_key) in observation.committee.iter_pairs() {
        hasher.update(network_key.encode());
        hasher.update(bls_key.encode());
    }
    RegistryStatus {
        mode: mode.as_str().to_string(),
        last_epoch: observation.epoch,
        last_lookahead_height: observation.lookahead_height,
        outcome: match observation.outcome {
            zksync_os_consensus_core::RecordedOutcome::Derived => "derived",
            zksync_os_consensus_core::RecordedOutcome::CarriedNoEntry => "carried_no_entry",
            zksync_os_consensus_core::RecordedOutcome::CarriedRefused => "carried_refused",
        }
        .to_string(),
        matches_config: observation.matches_config,
        refusal: observation.refusal.clone(),
        committee_hash: alloy::hex::encode(&Sha256::finalize(hasher)[..8]),
        committee_size: observation.committee.len(),
    }
}

/// Feeds consensus activity into metrics and the status tip. Fault evidence — proof a
/// committee member signed contradicting votes — is the loudest signal a validator
/// can produce: it must stay absent on a healthy committee.
#[derive(Clone)]
pub(super) struct ActivityObserver {
    pub(super) finalized: std::sync::Arc<tokio::sync::watch::Sender<Option<FinalizedObservation>>>,
    pub(super) finality: std::sync::Arc<zksync_os_consensus_execution::FinalityStore>,
    /// Certificates carry per-epoch signer bitmaps, and the custody records name
    /// per-epoch committees — both resolve through the committee source (which,
    /// under a registry flip, is more than the config schedule).
    pub(super) committees: CommitteeSource,
}

impl zksync_os_consensus_core::types::Reporter for ActivityObserver {
    type Activity = ConsensusActivity;

    fn report(&mut self, activity: Self::Activity) -> commonware_actor::Feedback {
        // Every vote or certificate names its round; the highest one ever seen is
        // persisted as the recovery floor for journal-loss restarts (see
        // `FinalityStore::note_observed_round`). Fault-evidence kinds are skipped —
        // their rounds ride inside the evidence pairs, and the votes they contain
        // were already observed individually.
        let observed_round = match &activity {
            Activity::Notarize(vote) => Some(vote.round()),
            Activity::Notarization(certificate) => Some(certificate.round()),
            Activity::Certification(certificate) => Some(certificate.round()),
            Activity::Nullify(vote) => Some(vote.round()),
            Activity::Nullification(certificate) => Some(certificate.round()),
            Activity::Finalize(vote) => Some(vote.round()),
            Activity::Finalization(finalization) => Some(finalization.round()),
            Activity::ConflictingNotarize(_)
            | Activity::ConflictingFinalize(_)
            | Activity::NullifyFinalize(_) => None,
        };
        if let Some(round) = observed_round
            && let Err(err) = self
                .finality
                .note_observed_round(round.epoch().get(), round.view().get())
        {
            tracing::error!(?err, "failed to persist the observed-round floor");
        }

        let kind = match &activity {
            Activity::Notarize(_) => "notarize",
            Activity::Notarization(_) => "notarization",
            Activity::Certification(_) => "certification",
            Activity::Nullify(_) => "nullify",
            Activity::Nullification(_) => "nullification",
            Activity::Finalize(_) => "finalize",
            Activity::Finalization(finalization) => {
                let round = finalization.round();
                let (epoch_committee, _) = self.committees.resolve(round.epoch());
                let committee_size = epoch_committee.len() as u32;
                // Finality is monotone, so the published observation must be too.
                // Finalizations do not arrive in round order here: the tip scout
                // re-hears certificates for already-retired epochs (a lagging peer
                // catching up re-broadcasts them, and with no engine registered for
                // that epoch they fall through to the scout), and marshal replays
                // finalizations during backfill. Without the clamp, a stale
                // re-heard finalization would move `/status.finalized` backwards on
                // a perfectly healthy validator. The durable observed-round floor
                // clamps internally already (`FinalityStore::note_observed_round`).
                let _ = self.finalized.send_if_modified(|current| {
                    let observed = (round.epoch().get(), round.view().get());
                    let advances = current
                        .as_ref()
                        .is_none_or(|seen| observed > (seen.epoch, seen.view));
                    if advances {
                        *current = Some(FinalizedObservation {
                            epoch: round.epoch().get(),
                            view: round.view().get(),
                            committee_size,
                            observed_unix: unix_now(),
                        });
                    }
                    advances
                });
                let block_digest: [u8; 32] = finalization
                    .proposal
                    .payload
                    .as_ref()
                    .try_into()
                    .expect("consensus digests are 32 bytes");
                // The sovereign copy: convert the certificate out of the consensus
                // library's types the moment it exists, so the durable record never
                // depends on the library's encoding staying stable.
                let signers: Vec<u32> = finalization
                    .certificate
                    .signers
                    .iter()
                    .map(|participant| participant.get())
                    .collect();
                let mut signature = Vec::new();
                commonware_codec::Write::write(&finalization.certificate.signature, &mut signature);
                let certificate = zksync_os_wire::FinalityCertificate {
                    scheme: zksync_os_wire::SignatureScheme::Bls12381Multisig,
                    epoch: round.epoch().get(),
                    view: round.view().get(),
                    block_digest,
                    committee_size,
                    signers: zksync_os_wire::FinalityCertificate::bitmap_from_positions(
                        committee_size,
                        &signers,
                    ),
                    signature,
                };
                if let Err(err) = self.finality.put_certificate(&certificate) {
                    tracing::error!(?err, "failed to persist a finality certificate");
                }
                // The floor cache: the same finalization in the consensus library's
                // own encoding, so a restart with empty consensus storage can hand
                // marshal a floor (the sovereign certificate cannot reconstruct
                // one). Cache semantics — see `FinalityCF::FloorCache`.
                {
                    use commonware_codec::Encode as _;
                    let raw = finalization.encode();
                    if let Err(err) = self.finality.put_raw_finalization(
                        round.epoch().get(),
                        round.view().get(),
                        block_digest,
                        raw.as_ref(),
                    ) {
                        tracing::error!(?err, "failed to cache a raw finalization");
                    }
                }
                // The custody trail: the first observed finalization of each epoch
                // records which committee holds it (first-observed wins; replays
                // change nothing).
                let transition = zksync_os_wire::EpochTransition {
                    epoch: round.epoch().get(),
                    scheme: zksync_os_wire::SignatureScheme::Bls12381Multisig,
                    committee: epoch_committee
                        .iter_pairs()
                        .map(|(network_key, bls_key)| {
                            use commonware_codec::Encode as _;
                            zksync_os_wire::CommitteeMemberKeys {
                                network_key: network_key
                                    .encode()
                                    .as_ref()
                                    .try_into()
                                    .expect("ed25519 public keys encode to 32 bytes"),
                                bls_key: bls_key
                                    .encode()
                                    .as_ref()
                                    .try_into()
                                    .expect("BLS12-381 MinPk public keys encode to 48 bytes"),
                            }
                        })
                        .collect(),
                    first_finalized_digest: block_digest,
                    first_finalized_view: round.view().get(),
                };
                match self.finality.record_epoch_transition(&transition) {
                    Ok(true) => {
                        tracing::info!(
                            epoch = transition.epoch,
                            committee_size,
                            "recorded committee custody entry for epoch"
                        );
                        // Keep the floor cache to the current and previous epoch —
                        // anything older fails the freshness policy anyway.
                        if let Some(keep_from) = transition.epoch.checked_sub(1)
                            && let Err(err) = self.finality.prune_raw_finalizations_below(keep_from)
                        {
                            tracing::warn!(?err, "failed to prune the floor cache");
                        }
                    }
                    Ok(false) => {}
                    Err(err) => {
                        tracing::error!(?err, "failed to persist an epoch transition record")
                    }
                }
                "finalization"
            }
            Activity::ConflictingNotarize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: conflicting notarize votes"
                );
                "conflicting_notarize"
            }
            Activity::ConflictingFinalize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: conflicting finalize votes"
                );
                "conflicting_finalize"
            }
            Activity::NullifyFinalize(evidence) => {
                tracing::warn!(
                    culprit = evidence.signer().get(),
                    "byzantine fault evidence: nullify and finalize in one view"
                );
                "nullify_finalize"
            }
        };
        CONSENSUS_METRICS.activity[&kind].inc();
        commonware_actor::Feedback::Ok
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
