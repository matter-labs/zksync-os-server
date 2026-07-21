//! Counting and the final verdict: per-validator and per-workload submission
//! counts, saga episode outcomes, and a receipt-sampling audit that checks
//! transactions met their workload's expectation. The audit is what turns
//! "load ran" into "the chain did the right thing with it" — `chaos load`
//! exits nonzero when it fails.

use super::bank::Bank;
use super::workloads::Expectation;
use alloy::primitives::B256;
use alloy::providers::Provider;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Recent submissions kept for the audit; old entries fall off — auditing the
/// tail of the run is the point (everything before it survived maintenance).
const LEDGER_CAP: usize = 1024;
/// Receipts sampled per workload by the audit.
const AUDIT_SAMPLE: usize = 5;

#[derive(Default, Clone)]
pub struct Counts {
    pub accepted: u64,
    pub rejected: u64,
}

/// How many distinct rejection reasons the report keeps (per whole run).
const REASON_CAP: usize = 8;
/// Reason strings are truncated: RPC errors embed whole transactions.
const REASON_PREFIX: usize = 120;

#[derive(Default, Clone)]
pub struct Episodes {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub notes: Vec<String>,
}

pub struct LedgerEntry {
    pub workload: &'static str,
    pub expect: Expectation,
    pub hash: B256,
    pub sender: usize,
}

pub struct LoadStats {
    pub per_validator: Vec<Counts>,
    pub per_workload: BTreeMap<&'static str, Counts>,
    pub episodes: BTreeMap<&'static str, Episodes>,
    pub ledger: VecDeque<LedgerEntry>,
    /// The first distinct rejection reasons seen, with counts — the difference
    /// between "validator 0 rejected 506" and knowing why.
    pub rejection_reasons: BTreeMap<String, u64>,
}

impl LoadStats {
    pub fn new(validators: usize) -> LoadStats {
        LoadStats {
            per_validator: vec![Counts::default(); validators],
            per_workload: BTreeMap::new(),
            episodes: BTreeMap::new(),
            ledger: VecDeque::new(),
            rejection_reasons: BTreeMap::new(),
        }
    }

    pub fn accepted(&mut self, entry: LedgerEntry, validator: usize) {
        self.per_validator[validator].accepted += 1;
        self.per_workload
            .entry(entry.workload)
            .or_default()
            .accepted += 1;
        self.ledger.push_back(entry);
        while self.ledger.len() > LEDGER_CAP {
            self.ledger.pop_front();
        }
    }

    pub fn rejected(&mut self, workload: &'static str, validator: usize, reason: &str) {
        self.per_validator[validator].rejected += 1;
        self.per_workload.entry(workload).or_default().rejected += 1;
        let key: String = reason.chars().take(REASON_PREFIX).collect();
        if self.rejection_reasons.len() < REASON_CAP || self.rejection_reasons.contains_key(&key) {
            *self.rejection_reasons.entry(key).or_insert(0) += 1;
        }
    }

    pub fn episode(&mut self, saga: &'static str, passed: bool, note: Option<String>) {
        let entry = self.episodes.entry(saga).or_default();
        if passed {
            entry.passed += 1;
        } else {
            entry.failed += 1;
            if let Some(note) = note
                && entry.notes.len() < 10
            {
                entry.notes.push(note);
            }
        }
    }

    pub fn skip(&mut self, saga: &'static str, note: String) {
        let entry = self.episodes.entry(saga).or_default();
        entry.skipped += 1;
        if entry.notes.len() < 10 {
            entry.notes.push(format!("skip: {note}"));
        }
    }

    /// A note on a counted episode (observed semantics worth reporting),
    /// without touching the verdict counters.
    pub fn note(&mut self, saga: &'static str, note: String) {
        let entry = self.episodes.entry(saga).or_default();
        if entry.notes.len() < 10 {
            entry.notes.push(note);
        }
    }
}

/// Prints the run report and audits a sample of receipts against workload
/// expectations. Errors when the chain contradicted a workload (a supposedly
/// clean transaction reverted, a planned failure succeeded, a saga saw a real
/// violation) — chaos-tolerant misses (still pending, unreachable) only count
/// as unconfirmed.
pub async fn final_report(stats: &LoadStats, bank: &Bank, elapsed: Duration) -> anyhow::Result<()> {
    let accepted: u64 = stats.per_validator.iter().map(|c| c.accepted).sum();
    let rejected: u64 = stats.per_validator.iter().map(|c| c.rejected).sum();
    println!("--- load report ({:.0}s) ---", elapsed.as_secs_f64());
    for (index, counts) in stats.per_validator.iter().enumerate() {
        if counts.accepted + counts.rejected > 0 {
            println!(
                "validator {index}: accepted {} rejected {}",
                counts.accepted, counts.rejected
            );
        }
    }
    for (workload, counts) in &stats.per_workload {
        println!(
            "workload {workload}: accepted {} rejected {}",
            counts.accepted, counts.rejected
        );
    }
    for (reason, count) in &stats.rejection_reasons {
        println!("rejections ({count}x): {reason}");
    }
    for (saga, episodes) in &stats.episodes {
        println!(
            "saga {saga}: {} passed, {} failed, {} skipped",
            episodes.passed, episodes.failed, episodes.skipped
        );
        for note in &episodes.notes {
            println!("  {note}");
        }
    }

    // End-to-end inclusion proof: the last accepted transaction of each sender
    // must land (they were priced by maintenance, so barring chaos they will).
    let mut included = 0usize;
    let mut missing = 0usize;
    for sender in &bank.senders {
        let Some(hash) = sender.last_hash else {
            continue;
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match sender.provider.get_transaction_receipt(hash).await {
                Ok(Some(_)) => {
                    included += 1;
                    break;
                }
                _ if Instant::now() >= deadline => {
                    missing += 1;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    // The expectation audit: newest AUDIT_SAMPLE entries per workload.
    let mut picked: BTreeMap<&'static str, Vec<&LedgerEntry>> = BTreeMap::new();
    for entry in stats.ledger.iter().rev() {
        let bucket = picked.entry(entry.workload).or_default();
        if bucket.len() < AUDIT_SAMPLE {
            bucket.push(entry);
        }
    }
    let mut audited = 0usize;
    let mut unconfirmed = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for entries in picked.values() {
        for entry in entries {
            let provider = &bank.senders[entry.sender].provider;
            match provider.get_transaction_receipt(entry.hash).await {
                Ok(Some(receipt)) => {
                    audited += 1;
                    let succeeded = receipt.status();
                    let expected_success = entry.expect == Expectation::Accept;
                    if succeeded != expected_success {
                        violations.push(format!(
                            "{}: {} expected {:?} but status was {}",
                            entry.workload,
                            entry.hash,
                            entry.expect,
                            if succeeded { "success" } else { "revert" },
                        ));
                    }
                }
                _ => unconfirmed += 1,
            }
        }
    }

    println!(
        "total accepted {accepted} ({:.1} tps), rejected {rejected}; \
         final txs included {included}, unconfirmed {missing}",
        accepted as f64 / elapsed.as_secs_f64().max(1.0),
    );
    println!(
        "expectation audit: {audited} receipts checked, {unconfirmed} unconfirmed, {} violations",
        violations.len(),
    );
    for violation in &violations {
        println!("  VIOLATION {violation}");
    }

    let episode_failures: u64 = stats.episodes.values().map(|e| e.failed).sum();
    anyhow::ensure!(
        violations.is_empty() && episode_failures == 0,
        "load audit failed: {} expectation violations, {episode_failures} saga failures",
        violations.len(),
    );
    Ok(())
}
