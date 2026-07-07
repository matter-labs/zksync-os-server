//! The deposits saga: a steady trickle of real L1→L2 priority operations, with
//! an occasional burst — the L1 watcher's relay path exercised continuously
//! rather than once at funding time.
//!
//! Verdicts are chaos-tolerant with one sharpening: a deposit that fails to
//! arrive is only a *failure* if the L2 chain demonstrably kept producing
//! blocks while we waited (the relay went missing); a stalled or unreachable
//! chain makes it a skip.

use super::l1_support::L1Side;
use crate::load::stats::LoadStats;
use alloy::primitives::{Address, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SAGA: &str = "deposits";
/// How long a deposit may take to be relayed and credited on a live chain.
const ARRIVAL_DEADLINE: Duration = Duration::from_secs(150);
/// The chain must have advanced at least this many blocks during the wait for
/// a missing credit to count as a real relay failure.
const LIVE_BLOCKS_FOR_VERDICT: u64 = 40;

pub struct Deposits {
    l1: L1Side,
    l2: DynProvider,
    episode: u64,
}

impl Deposits {
    pub fn new(l1: L1Side, l2: DynProvider) -> Deposits {
        Deposits { l1, l2, episode: 0 }
    }

    pub async fn run(
        mut self,
        every: Duration,
        stats: Arc<Mutex<LoadStats>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => return,
            }
            self.episode += 1;
            // Every fifth episode is a small burst: priority ops arriving
            // together, the way bridge traffic actually looks.
            let count = if self.episode.is_multiple_of(5) { 3 } else { 1 };
            match self.deposit_round(count).await {
                Ok(None) => stats.lock().unwrap().episode(SAGA, true, None),
                Ok(Some(skip)) => {
                    println!("deposits episode {}: skipped — {skip}", self.episode);
                    stats.lock().unwrap().skip(SAGA, skip);
                }
                Err(reason) => {
                    println!("deposits episode {}: FAILED — {reason}", self.episode);
                    stats.lock().unwrap().episode(SAGA, false, Some(reason));
                }
            }
        }
    }

    /// `Ok(None)` = pass, `Ok(Some(reason))` = skip, `Err(reason)` = failure.
    async fn deposit_round(&mut self, count: usize) -> Result<Option<String>, String> {
        let amount = U256::from(10_000_000_000_000_000u128); // 0.01 ETH
        let mut recipients = Vec::new();
        for lane in 0..count {
            let mut material = b"chaos-deposit".to_vec();
            material.extend_from_slice(&self.episode.to_be_bytes());
            material.extend_from_slice(&(lane as u64).to_be_bytes());
            let recipient = Address::from_word(keccak256(material));
            match self
                .l1
                .deposit(recipient, amount, vec![], 300_000, recipient)
                .await
            {
                Ok(_l2_hash) => recipients.push(recipient),
                // An unreachable or refusing L1 is sanctioned chaos (blackouts).
                Err(error) => return Ok(Some(format!("L1 submission failed: {error:#}"))),
            }
        }

        let start_block = self.l2.get_block_number().await.unwrap_or(0);
        let deadline = tokio::time::Instant::now() + ARRIVAL_DEADLINE;
        let mut pending = recipients;
        while !pending.is_empty() {
            if tokio::time::Instant::now() >= deadline {
                let end_block = self.l2.get_block_number().await.unwrap_or(start_block);
                let advanced = end_block.saturating_sub(start_block);
                return if advanced >= LIVE_BLOCKS_FOR_VERDICT {
                    Err(format!(
                        "{} deposit(s) not credited although the chain advanced {advanced} blocks",
                        pending.len(),
                    ))
                } else {
                    Ok(Some(format!(
                        "unresolved: chain advanced only {advanced} blocks in the window \
                         (stalled or catching up)",
                    )))
                };
            }
            let mut still_pending = Vec::new();
            for recipient in pending {
                match self.l2.get_balance(recipient).await {
                    // The recipient doubles as the refund recipient, so the
                    // credit is the value plus the unused-gas refund.
                    Ok(balance) if balance >= amount => {}
                    Ok(balance) if !balance.is_zero() => {
                        return Err(format!(
                            "deposit credited too little: {balance} of {amount}",
                        ));
                    }
                    _ => still_pending.push(recipient),
                }
            }
            pending = still_pending;
            if !pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
        }
        Ok(None)
    }
}
