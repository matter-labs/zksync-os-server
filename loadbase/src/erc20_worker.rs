//! ERC‑20 worker; now submits **batches of 10 signed txs** via JSON‑RPC.
//! Adds gas‑price (legacy) so nodes don’t reject with “feeCap 0 below chain minimum”.

use crate::{erc20::SimpleERC20, metrics::Metrics};
use ethers::{
    prelude::*,
    types::{Bytes, U256},
};
use hex::encode as hex_encode;
use parking_lot::RwLock;
use rand::{rngs::StdRng, seq::SliceRandom};
use rand_distr::{Distribution, Normal};
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const JITTER_SIGMA: f64 = 0.20;
const BATCH_SIZE: usize = 10;
const GAS_WINDOW_SIZE: usize = 24;
const MIN_BID_BPS: u32 = 400;
const MAX_BID_BPS: u32 = 5000;
const STALL_GROWTH_BPS: u32 = 300;
const RECOVERY_DECAY_BPS: u32 = 120;

type EthSigner = SignerMiddleware<Provider<Http>, LocalWallet>;

struct PendingTx {
    raw: Bytes,
    global_permit: tokio::sync::OwnedSemaphorePermit,
    wallet_permit: tokio::sync::OwnedSemaphorePermit,
    sent_at: Instant,
    nonce: u64,
}

pub struct WorkerConfig {
    pub gas_limit: U256,
    pub mean_amt: U256,
    pub token_addr: Address,
    pub dest_random: bool,
    pub rpc_url: String,
    pub all_addrs: Vec<Address>,
    pub rng: Arc<RwLock<StdRng>>,
}

fn jitter_amount(mean: U256, rng: &RwLock<StdRng>) -> U256 {
    let delta = {
        let mut g = rng.write();
        Normal::new(0.0, JITTER_SIGMA).unwrap().sample(&mut *g)
    };
    if delta == 0.0 {
        return mean;
    }
    let d = U256::from((mean.as_u128() as f64 * delta.abs()) as u128);
    if delta.is_sign_positive() {
        mean + d
    } else {
        mean - d
    }
}

fn choose_dest(
    dest_random: bool,
    all_addrs: &[Address],
    self_addr: Address,
    rng: &RwLock<StdRng>,
) -> Address {
    if dest_random {
        return H160::random();
    }
    loop {
        let cand = {
            let mut g = rng.write();
            *all_addrs.choose(&mut *g).unwrap()
        };
        if cand != self_addr {
            return cand;
        }
    }
}

struct AdaptiveGasBidder {
    recent: VecDeque<U256>,
    stress_bps: u32,
}

impl AdaptiveGasBidder {
    fn new() -> Self {
        Self {
            recent: VecDeque::new(),
            stress_bps: MIN_BID_BPS,
        }
    }

    fn next_bid(&mut self, current: U256, inclusion_progress: bool, in_flight_saturated: bool) -> U256 {
        self.recent.push_back(current);
        while self.recent.len() > GAS_WINDOW_SIZE {
            self.recent.pop_front();
        }

        if in_flight_saturated && !inclusion_progress {
            self.stress_bps = self.stress_bps.saturating_add(STALL_GROWTH_BPS).min(MAX_BID_BPS);
        } else {
            self.stress_bps = self.stress_bps.saturating_sub(RECOVERY_DECAY_BPS).max(MIN_BID_BPS);
        }

        let volatility_bps = self.volatility_bps(current);
        let effective_bps = self.stress_bps.max(volatility_bps);
        // ceil(current * bps / 100)
        current
            .saturating_mul(U256::from(effective_bps))
            .saturating_add(U256::from(99u64))
            / U256::from(100u64)
    }

    fn volatility_bps(&self, current: U256) -> u32 {
        if self.recent.len() < 3 || current.is_zero() {
            return MIN_BID_BPS;
        }
        let mut sorted: Vec<U256> = self.recent.iter().copied().collect();
        sorted.sort_unstable();
        let idx = ((sorted.len() * 9).div_ceil(10)).saturating_sub(1);
        let p90 = sorted[idx];
        let ratio_bps_u256 = p90
            .saturating_mul(U256::from(100u64))
            .saturating_add(current.saturating_sub(U256::one()))
            / current;
        let ratio_bps = ratio_bps_u256.as_u32();
        ratio_bps.clamp(MIN_BID_BPS, MAX_BID_BPS)
    }
}

async fn build_batch(
    signer: &EthSigner,
    token: &SimpleERC20<EthSigner>,
    global_sem: &Arc<Semaphore>,
    wallet_sem: &Arc<Semaphore>,
    nonce: &mut u64,
    gas_price: U256,
    cfg: &WorkerConfig,
) -> Vec<PendingTx> {
    let mut batch = Vec::new();

    for _ in 0..BATCH_SIZE {
        let global_permit = match global_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => break, // in‑flight limit
        };
        let wallet_permit = match wallet_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                drop(global_permit);
                break; // only one in-flight tx per wallet
            }
        };

        let dest = choose_dest(cfg.dest_random, &cfg.all_addrs, signer.address(), &cfg.rng);
        let amt = jitter_amount(cfg.mean_amt, &cfg.rng);

        let mut call = token.transfer(dest, amt);
        call.tx.set_gas(cfg.gas_limit);
        call.tx.set_gas_price(gas_price);
        let tx_nonce = *nonce;
        call.tx.set_nonce(tx_nonce);
        *nonce += 1;

        let sig = signer
            .signer()
            .sign_transaction(&call.tx)
            .await
            .expect("sign");
        let raw = call.tx.rlp_signed(&sig);

        batch.push(PendingTx {
            raw,
            global_permit,
            wallet_permit,
            sent_at: Instant::now(),
            nonce: tx_nonce,
        });
    }

    batch
}

fn spawn_receipt_waiter(
    tx_hash: H256,
    global_permit: tokio::sync::OwnedSemaphorePermit,
    wallet_permit: tokio::sync::OwnedSemaphorePermit,
    provider: Provider<Http>,
    metrics: Metrics,
    nonce_refresh: Arc<AtomicBool>,
) {
    const RECEIPT_TIMEOUT: Duration = Duration::from_secs(5);

    tokio::spawn(async move {
        let t_inc = Instant::now();
        loop {
            if t_inc.elapsed() >= RECEIPT_TIMEOUT {
                metrics.record_receipt_timeout();
                nonce_refresh.store(true, Ordering::Relaxed);
                eprintln!(
                    "tx {tx_hash:?} unconfirmed for {}s - node dropped it",
                    RECEIPT_TIMEOUT.as_secs()
                );
                break;
            }
            match provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(_)) => {
                    metrics.record_included(t_inc.elapsed().as_millis() as u64);
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => {
                    metrics.record_receipt_error();
                    nonce_refresh.store(true, Ordering::Relaxed);
                    eprintln!("receipt poll error for {tx_hash:?}: {e}");
                    break;
                }
            }
        }
        drop(wallet_permit);
        drop(global_permit);
    });
}

fn should_refresh_nonce_for_rpc_error(err: &Value) -> bool {
    let msg = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    msg.contains("nonce too low")
        || msg.contains("nonce too high")
        || msg.contains("already known")
        || msg.contains("known transaction")
        || msg.contains("replacement transaction underpriced")
        || msg.contains("transaction underpriced")
}

fn process_replies(
    batch: Vec<PendingTx>,
    replies: Vec<Value>,
    provider: &Provider<Http>,
    metrics: &Metrics,
    nonce_refresh: &Arc<AtomicBool>,
) {
    for (tx, reply) in batch.into_iter().zip(replies) {
        let sub_ms = tx.sent_at.elapsed().as_millis() as u64;

        if let Some(tx_hash_str) = reply.get("result").and_then(|v| v.as_str()) {
            let tx_hash: H256 = tx_hash_str.parse().unwrap_or_default();
            metrics.record_submitted(sub_ms);
            spawn_receipt_waiter(
                tx_hash,
                tx.global_permit,
                tx.wallet_permit,
                provider.clone(),
                metrics.clone(),
                nonce_refresh.clone(),
            );
        } else {
            if let Some(err) = reply.get("error") {
                if should_refresh_nonce_for_rpc_error(err) {
                    nonce_refresh.store(true, Ordering::Relaxed);
                }
                eprintln!("❗ tx error for nonce {}: {err}", tx.nonce);
            }
            // tx.permit dropped here, freeing the slot
        }
    }
}

async fn send_rpc_batch(http: &Client, url: &str, batch: &[PendingTx]) -> Option<Vec<Value>> {
    let payload: Vec<_> = batch
        .iter()
        .enumerate()
        .map(|(i, tx)| {
            json!({
                "jsonrpc": "2.0",
                "id":      i,
                "method":  "eth_sendRawTransaction",
                "params":  [format!("0x{}", hex_encode(&tx.raw))]
            })
        })
        .collect();

    let resp = http
        .post(url)
        .json(&payload)
        .send()
        .await
        .inspect_err(|e| eprintln!("❗ batch send error {e}"))
        .ok()?;

    resp.json::<Vec<Value>>()
        .await
        .inspect_err(|e| eprintln!("❗ bad JSON reply {e}"))
        .ok()
}

async fn run_wallet(
    idx: usize,
    wallet: LocalWallet,
    provider: Provider<Http>,
    sem: Arc<Semaphore>,
    metrics: Metrics,
    running: Arc<AtomicBool>,
    http: Arc<Client>,
    cfg: Arc<WorkerConfig>,
) {
    let signer = SignerMiddleware::new(provider.clone(), wallet);
    let token = SimpleERC20::new(cfg.token_addr, Arc::new(signer.clone()));

    let mut nonce: u64 = signer
        .get_transaction_count(signer.address(), Some(BlockNumber::Pending.into()))
        .await
        .expect("nonce")
        .as_u64();
    let mut gas_bidder = AdaptiveGasBidder::new();
    let mut last_included_total = 0u64;
    let nonce_refresh = Arc::new(AtomicBool::new(false));
    let wallet_sem = Arc::new(Semaphore::new(1));
    println!("erc20 wallet {idx} start‑nonce {nonce}");

    while running.load(Ordering::Relaxed) {
        if nonce_refresh.swap(false, Ordering::Relaxed) {
            match signer
                .get_transaction_count(signer.address(), Some(BlockNumber::Pending.into()))
                .await
            {
                Ok(chain_nonce) => {
                    let chain_nonce = chain_nonce.as_u64();
                    if chain_nonce != nonce {
                        eprintln!(
                            "erc20 wallet {idx} nonce resync: local={nonce}, chain_pending={chain_nonce}"
                        );
                        nonce = chain_nonce;
                    }
                }
                Err(e) => {
                    eprintln!("❗ nonce refresh failed for wallet {idx}: {e}");
                }
            }
        }

        let chain_gas_price = match provider.get_gas_price().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("❗ gas‑price fetch error {e} – using 3 gwei");
                U256::from(3_000_000_000u64) // 3 gwei fallback
            }
        };
        let included_now = metrics.total_included();
        let inclusion_progress = included_now > last_included_total;
        last_included_total = included_now;
        let in_flight_saturated = sem.available_permits() == 0;
        let gas_price =
            gas_bidder.next_bid(chain_gas_price, inclusion_progress, in_flight_saturated);

        let batch = build_batch(
            &signer,
            &token,
            &sem,
            &wallet_sem,
            &mut nonce,
            gas_price,
            &cfg,
        )
        .await;

        if batch.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        let Some(replies) = send_rpc_batch(&http, &cfg.rpc_url, &batch).await else {
            continue;
        };

        process_replies(batch, replies, &provider, &metrics, &nonce_refresh);
    }
}

pub fn spawn_erc20_workers(
    provider: Provider<Http>,
    wallets: Vec<LocalWallet>,
    metrics: Metrics,
    running: Arc<AtomicBool>,
    max_in_flight: u32,
    cfg: WorkerConfig,
) -> Vec<tokio::task::JoinHandle<()>> {
    let cfg = Arc::new(cfg);
    let http = Arc::new(Client::new());
    let sem = Arc::new(Semaphore::new(max_in_flight as usize));

    wallets
        .into_iter()
        .enumerate()
        .map(|(idx, wallet)| {
            tokio::spawn(run_wallet(
                idx,
                wallet,
                provider.clone(),
                sem.clone(),
                metrics.clone(),
                running.clone(),
                http.clone(),
                cfg.clone(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AdaptiveGasBidder, should_refresh_nonce_for_rpc_error};
    use ethers::types::U256;
    use serde_json::json;

    #[test]
    fn nonce_related_errors_trigger_refresh() {
        assert!(should_refresh_nonce_for_rpc_error(
            &json!({"code": -32000, "message": "nonce too low"})
        ));
        assert!(should_refresh_nonce_for_rpc_error(
            &json!({"code": -32000, "message": "replacement transaction underpriced"})
        ));
        assert!(should_refresh_nonce_for_rpc_error(
            &json!({"code": -32000, "message": "already known"})
        ));
    }

    #[test]
    fn unrelated_errors_do_not_trigger_refresh() {
        assert!(!should_refresh_nonce_for_rpc_error(
            &json!({"code": -32602, "message": "invalid params"})
        ));
    }

    #[test]
    fn adaptive_bidder_increases_bid_on_sustained_stall() {
        let mut bidder = AdaptiveGasBidder::new();
        let base = U256::from(100u64);
        let healthy = bidder.next_bid(base, true, false);
        let stalled1 = bidder.next_bid(base, false, true);
        let stalled2 = bidder.next_bid(base, false, true);
        assert!(stalled1 > healthy);
        assert!(stalled2 > stalled1);
    }

    #[test]
    fn adaptive_bidder_tracks_recent_spikes_without_stall() {
        let mut bidder = AdaptiveGasBidder::new();
        let _ = bidder.next_bid(U256::from(100u64), true, false);
        let _ = bidder.next_bid(U256::from(220u64), true, false);
        let bid = bidder.next_bid(U256::from(100u64), true, false);
        assert!(bid >= U256::from(180u64));
    }

    #[test]
    fn adaptive_bidder_decays_after_recovery() {
        let mut bidder = AdaptiveGasBidder::new();
        let base = U256::from(100u64);
        let _ = bidder.next_bid(base, true, false);
        let high = bidder.next_bid(base, false, true);
        let recovered1 = bidder.next_bid(base, true, false);
        let recovered2 = bidder.next_bid(base, true, false);
        assert!(recovered1 <= high);
        assert!(recovered2 <= recovered1);
    }
}
