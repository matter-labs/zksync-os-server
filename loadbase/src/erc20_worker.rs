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
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const JITTER_SIGMA: f64 = 0.20;
const BATCH_SIZE: usize = 10;

type EthSigner = SignerMiddleware<Provider<Http>, LocalWallet>;

struct PendingTx {
    raw: Bytes,
    permit: tokio::sync::OwnedSemaphorePermit,
    sent_at: Instant,
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

async fn build_batch(
    signer: &EthSigner,
    token: &SimpleERC20<EthSigner>,
    sem: &Arc<Semaphore>,
    nonce: &mut u64,
    gas_price: U256,
    cfg: &WorkerConfig,
) -> Vec<PendingTx> {
    let mut batch = Vec::new();

    for _ in 0..BATCH_SIZE {
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => break, // in‑flight limit
        };

        let dest = choose_dest(cfg.dest_random, &cfg.all_addrs, signer.address(), &cfg.rng);
        let amt = jitter_amount(cfg.mean_amt, &cfg.rng);

        let mut call = token.transfer(dest, amt);
        call.tx.set_gas(cfg.gas_limit);
        call.tx.set_gas_price(gas_price); // **the fix**
        call.tx.set_nonce(*nonce);
        *nonce += 1;

        let sig = signer
            .signer()
            .sign_transaction(&call.tx)
            .await
            .expect("sign");
        let raw = call.tx.rlp_signed(&sig);

        batch.push(PendingTx {
            raw,
            permit,
            sent_at: Instant::now(),
        });
    }

    batch
}

fn spawn_receipt_waiter(
    tx_hash: H256,
    permit: tokio::sync::OwnedSemaphorePermit,
    provider: Provider<Http>,
    metrics: Metrics,
) {
    const RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);

    tokio::spawn(async move {
        let t_inc = Instant::now();
        loop {
            if t_inc.elapsed() >= RECEIPT_TIMEOUT {
                metrics.record_receipt_timeout();
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
                    eprintln!("receipt poll error for {tx_hash:?}: {e}");
                    break;
                }
            }
        }
        drop(permit); // free slot
    });
}

fn process_replies(
    batch: Vec<PendingTx>,
    replies: Vec<Value>,
    provider: &Provider<Http>,
    metrics: &Metrics,
) {
    for (tx, reply) in batch.into_iter().zip(replies) {
        let sub_ms = tx.sent_at.elapsed().as_millis() as u64;

        if let Some(tx_hash_str) = reply.get("result").and_then(|v| v.as_str()) {
            let tx_hash: H256 = tx_hash_str.parse().unwrap_or_default();
            metrics.record_submitted(sub_ms);
            spawn_receipt_waiter(tx_hash, tx.permit, provider.clone(), metrics.clone());
        } else {
            if let Some(err) = reply.get("error") {
                eprintln!("❗ tx error {err}");
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
    println!("erc20 wallet {idx} start‑nonce {nonce}");

    while running.load(Ordering::Relaxed) {
        let gas_price = match provider.get_gas_price().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("❗ gas‑price fetch error {e} – using 3 gwei");
                U256::from(3_000_000_000u64) // 3 gwei fallback
            }
        };

        let batch = build_batch(&signer, &token, &sem, &mut nonce, gas_price, &cfg).await;

        if batch.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        let Some(replies) = send_rpc_batch(&http, &cfg.rpc_url, &batch).await else {
            continue;
        };

        process_replies(batch, replies, &provider, &metrics);
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
