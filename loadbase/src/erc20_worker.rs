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
    raw:     Bytes,
    permit:  tokio::sync::OwnedSemaphorePermit,
    sent_at: Instant,
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
    if delta.is_sign_positive() { mean + d } else { mean - d }
}

fn choose_dest(dest_random: bool, all_addrs: &[Address], self_addr: Address, rng: &RwLock<StdRng>) -> Address {
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
    signer:      &EthSigner,
    token:       &SimpleERC20<Arc<EthSigner>>,
    sem:         &Arc<Semaphore>,
    nonce:       &mut U256,
    gas_price:   U256,
    gas_limit:   U256,
    mean_amt:    U256,
    dest_random: bool,
    all_addrs:   &[Address],
    rng:         &RwLock<StdRng>,
) -> Vec<PendingTx> {
    let mut batch = Vec::<PendingTx>::new();

    for _ in 0..BATCH_SIZE {
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p)  => p,
            Err(_) => break, // in‑flight limit
        };

        let dest = choose_dest(dest_random, all_addrs, signer.address(), rng);
        let amt  = jitter_amount(mean_amt, rng);

        let mut call = token.transfer(dest, amt);
        call.tx.set_gas(gas_limit);
        call.tx.set_gas_price(gas_price); // **the fix**
        call.tx.set_nonce(*nonce);
        *nonce += U256::one();

        let sig = signer.signer().sign_transaction(&call.tx).await.expect("sign");
        let raw = call.tx.rlp_signed(&sig);

        batch.push(PendingTx { raw, permit, sent_at: Instant::now() });
    }

    batch
}

fn spawn_receipt_waiter(
    tx_hash:  H256,
    permit:   tokio::sync::OwnedSemaphorePermit,
    provider: Provider<Http>,
    metrics:  Metrics,
) {
    tokio::spawn(async move {
        let t_inc = Instant::now();
        loop {
            match provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(_)) => {
                    let inc = t_inc.elapsed().as_millis() as u64;
                    metrics.include.write().record(inc).ok();
                    metrics.inc_last.lock().push_back((Instant::now(), inc));
                    metrics.included.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(_)   => break,
            }
        }
        drop(permit); // free slot
    });
}

fn process_replies(
    batch:    Vec<PendingTx>,
    replies:  Vec<Value>,
    provider: &Provider<Http>,
    metrics:  &Metrics,
) {
    for (tx, reply) in batch.into_iter().zip(replies) {
        let sub_ms = tx.sent_at.elapsed().as_millis() as u64;

        if let Some(tx_hash_str) = reply.get("result").and_then(|v| v.as_str()) {
            let tx_hash: H256 = tx_hash_str.parse().unwrap_or_default();
            metrics.submit.write().record(sub_ms).ok();
            metrics.sub_last.lock().push_back((Instant::now(), sub_ms));
            metrics.sent.fetch_add(1, Ordering::Relaxed);
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

    let resp = http.post(url).json(&payload).send().await
        .map_err(|e| eprintln!("❗ batch send error {e}"))
        .ok()?;

    resp.json::<Vec<Value>>().await
        .map_err(|e| eprintln!("❗ bad JSON reply {e}"))
        .ok()
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_erc20_workers(
    provider: Provider<Http>,
    wallets: Vec<LocalWallet>,
    gas_limit: U256,
    metrics: Metrics,
    running: Arc<AtomicBool>,
    max_in_flight: u32,
    mean_amt: U256,
    token_addr: Address,
    rng: Arc<RwLock<StdRng>>,
    dest_random: bool,
    rpc_url: String,
) -> Vec<tokio::task::JoinHandle<()>> {
    let addrs: Vec<_> = wallets.iter().map(|w| w.address()).collect();
    let sems = (0..wallets.len())
        .map(|_| Arc::new(Semaphore::new(max_in_flight as usize)))
        .collect::<Vec<_>>();
    let http = Arc::new(Client::new());

    wallets
        .into_iter()
        .enumerate()
        .map(|(idx, wallet)| {
            let sem         = sems[idx].clone();
            let provider_c  = provider.clone();
            let addrs_c     = addrs.clone();
            let m           = metrics.clone();
            let running_c   = running.clone();
            let rng_c       = rng.clone();
            let gas_limit_c = gas_limit;
            let token_addr_c= token_addr;
            let dest_rand   = dest_random;
            let rpc_url_c   = rpc_url.clone();
            let http_c      = http.clone();

            tokio::spawn(async move {
                let signer = SignerMiddleware::new(provider_c.clone(), wallet.clone());
                let token  = SimpleERC20::new(token_addr_c, Arc::new(signer.clone()));

                let mut nonce = signer
                    .get_transaction_count(signer.address(), Some(BlockNumber::Pending.into()))
                    .await
                    .expect("nonce");
                println!("erc20 wallet {idx} start‑nonce {nonce}");

                while running_c.load(Ordering::Relaxed) {
                    //----------------------------------------------//
                    // 0. fetch gas‑price once per batch            //
                    //----------------------------------------------//
                    let gas_price = match provider_c.get_gas_price().await {
                        Ok(p)  => p,
                        Err(e) => {
                            eprintln!("❗ gas‑price fetch error {e} – using 3 gwei");
                            U256::from(3_000_000_000u64) // 3 gwei fallback
                        }
                    };

                    //----------------------------------------------//
                    // 1. build ≤BATCH_SIZE signed raw txs          //
                    //----------------------------------------------//
                    let batch = build_batch(
                        &signer, &token, &sem, &mut nonce,
                        gas_price, gas_limit_c, mean_amt,
                        dest_rand, &addrs_c, &rng_c,
                    ).await;

                    if batch.is_empty() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    //----------------------------------------------//
                    // 2. send JSON‑RPC batch                       //
                    //----------------------------------------------//
                    let Some(replies) = send_rpc_batch(&http_c, &rpc_url_c, &batch).await else {
                        continue;
                    };

                    //----------------------------------------------//
                    // 3. per‑tx accounting & receipt waiters       //
                    //----------------------------------------------//
                    process_replies(batch, replies, &provider_c, &m);
                }
            })
        })
        .collect()
}
