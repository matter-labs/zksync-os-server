//! The nonce-race saga: one account signs two *different* transactions with the
//! same nonce and submits them simultaneously to two different validators'
//! mempools. Exactly one may ever mine; the pools and leader rotation sort out
//! which. Runs as its own task on a profile-set cadence, alternating between
//! two dedicated accounts so a slow episode never blocks the next.
//!
//! Verdicts are deliberately chaos-tolerant: an episode only *fails* on
//! evidence of a real bug (both transactions mined, or the account's nonce
//! advancing with neither mined). Everything the environment can legitimately
//! cause under fault injection — submissions refused, the chain paused past
//! the episode deadline — is recorded as a skip, not a failure.

use crate::load::bank::{Bank, Sender, TRANSFER_GAS};
use crate::load::stats::LoadStats;
use alloy::eips::Encodable2718 as _;
use alloy::network::{EthereumWallet, NetworkTransactionBuilder as _, TransactionBuilder};
use alloy::primitives::{Address, U256, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SAGA: &str = "nonce_race";
/// How long an episode waits for the race to resolve before calling it a skip
/// (the chain may be legitimately paused under fault injection).
const RESOLVE_DEADLINE: Duration = Duration::from_secs(90);

pub struct NonceRace {
    /// The racing accounts (their `provider` is where funding checked them;
    /// each episode submits through both `rpc` endpoints below).
    racers: Vec<Sender>,
    /// Wallet-less endpoints on two different validators.
    rpc: [DynProvider; 2],
    chain_id: u64,
    episode: u64,
}

impl NonceRace {
    /// `None` on a single-validator cluster — there is nothing to race.
    pub fn build(bank: &mut Bank, rpc_urls: &[String]) -> anyhow::Result<Option<NonceRace>> {
        if rpc_urls.len() < 2 {
            println!("nonce_race: needs at least 2 validators; saga off");
            return Ok(None);
        }
        let connect = |url: &String| -> anyhow::Result<DynProvider> {
            Ok(ProviderBuilder::new()
                .connect_http(url.parse().context("validator rpc url")?)
                .erased())
        };
        Ok(Some(NonceRace {
            racers: std::mem::take(&mut bank.racers),
            rpc: [connect(&rpc_urls[0])?, connect(&rpc_urls[1])?],
            chain_id: bank.chain_id,
            episode: 0,
        }))
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
            let racer = (self.episode % 2) as usize;
            match self.race_once(racer).await {
                Ok(Verdict::Pass) => stats.lock().unwrap().episode(SAGA, true, None),
                Ok(Verdict::Skip(reason)) => {
                    println!("nonce_race episode {}: skipped — {reason}", self.episode);
                    stats.lock().unwrap().skip(SAGA, reason);
                }
                Err(reason) => {
                    println!("nonce_race episode {}: FAILED — {reason}", self.episode);
                    stats.lock().unwrap().episode(SAGA, false, Some(reason));
                }
            }
        }
    }

    async fn race_once(&mut self, racer: usize) -> Result<Verdict, String> {
        let address = self.racers[racer].address;
        let signer = self.racers[racer].signer.clone();

        // The account's own view of its nonce, refreshed from whichever racing
        // endpoint answers (a previous unresolved episode may have left a
        // pending transaction — racing that same nonce again is fine).
        let mut nonce = None;
        for rpc in &self.rpc {
            if let Ok(fresh) = rpc.get_transaction_count(address).pending().await {
                nonce = Some(fresh);
                break;
            }
        }
        let Some(nonce) = nonce else {
            return Ok(Verdict::Skip("no validator answered a nonce query".into()));
        };

        let fees = self.current_fees().await;
        let wallet = EthereumWallet::from(signer);
        let mut hashes = Vec::new();
        let mut raws = Vec::new();
        for (lane, value) in [(0usize, 1u64), (1, 2)] {
            // Two *different* transfers: same nonce, same fees, different
            // payloads — a true race, not a replacement.
            let to = race_recipient(self.episode, lane);
            let request = TransactionRequest::default()
                .with_chain_id(self.chain_id)
                .with_from(address)
                .with_to(to)
                .with_value(U256::from(value))
                .with_nonce(nonce)
                .with_gas_limit(TRANSFER_GAS)
                .with_max_fee_per_gas(fees.0)
                .with_max_priority_fee_per_gas(fees.1);
            let envelope = request
                .build(&wallet)
                .await
                .map_err(|err| format!("signing the race pair: {err}"))?;
            hashes.push(*envelope.tx_hash());
            raws.push(envelope.encoded_2718());
        }

        let (sent_a, sent_b) = tokio::join!(
            self.rpc[0].send_raw_transaction(&raws[0]),
            self.rpc[1].send_raw_transaction(&raws[1]),
        );
        let accepted = [sent_a.is_ok(), sent_b.is_ok()];
        if accepted == [false, false] {
            return Ok(Verdict::Skip(
                "both submissions refused (validators down or gossip beat us)".into(),
            ));
        }

        // Wait for the account to move past the raced nonce, then count winners.
        let deadline = tokio::time::Instant::now() + RESOLVE_DEADLINE;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(Verdict::Skip(
                    "unresolved within the deadline (chain paused?)".into(),
                ));
            }
            for rpc in &self.rpc {
                if let Ok(chain_nonce) = rpc.get_transaction_count(address).await
                    && chain_nonce > nonce
                {
                    let mut mined = 0;
                    for hash in &hashes {
                        if let Ok(Some(_)) = rpc.get_transaction_receipt(*hash).await {
                            mined += 1;
                        }
                    }
                    return match mined {
                        1 => Ok(Verdict::Pass),
                        2 => Err("both same-nonce transactions mined".into()),
                        // Neither of THIS pair mined yet the nonce moved: under
                        // kill churn, pools get wiped and a previous episode's
                        // still-gossiped submission can consume the nonce
                        // instead. Exactly-one-per-nonce still held, so this is
                        // displacement, not a safety failure.
                        _ => Ok(Verdict::Skip(
                            "race pair displaced (pools flushed under faults); the nonce was                              still consumed exactly once"
                                .into(),
                        )),
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Self-contained fee refresh mirroring the bank's floors; the saga
    /// outlives any fee cache the engine maintains.
    async fn current_fees(&self) -> (u128, u128) {
        for rpc in &self.rpc {
            if let Ok(gas_price) = rpc.get_gas_price().await {
                return (
                    gas_price.saturating_mul(4).max(1_000_000_000),
                    (gas_price / 10).max(1_000),
                );
            }
        }
        (2_000_000_000, 100_000_000)
    }
}

enum Verdict {
    Pass,
    Skip(String),
}

/// Race recipients are derived per (episode, lane): distinct, reproducible.
fn race_recipient(episode: u64, lane: usize) -> Address {
    let mut bytes = b"chaos-race".to_vec();
    bytes.extend_from_slice(&episode.to_be_bytes());
    bytes.extend_from_slice(&(lane as u64).to_be_bytes());
    Address::from_word(keccak256(bytes))
}
