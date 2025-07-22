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
use serde_json::json;
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
    let normal = Normal::new(0.0, JITTER_SIGMA).unwrap();
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
            let normal_c    = normal;
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
                    let mut batch_raw    = Vec::<Bytes>::new();
                    let mut batch_permit = Vec::<tokio::sync::OwnedSemaphorePermit>::new();
                    let mut send_start   = Vec::<Instant>::new();

                    for _ in 0..BATCH_SIZE {
                        let permit = match sem.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => break, // in‑flight limit
                        };

                        // choose dest
                        let dest = if dest_rand {
                            H160::random()
                        } else {
                            loop {
                                let cand = {
                                    let mut g = rng_c.write();
                                    *addrs_c.choose(&mut *g).unwrap()
                                };
                                if cand != signer.address() {
                                    break cand;
                                }
                            }
                        };

                        // jitter amount
                        let delta = {
                            let mut g = rng_c.write();
                            normal_c.sample(&mut *g)
                        };
                        let mut amt = mean_amt;
                        if delta != 0.0 {
                            let d = U256::from((mean_amt.as_u128() as f64 * delta.abs()) as u128);
                            amt = if delta.is_sign_positive() { amt + d } else { amt - d };
                        }

                        // craft + sign
                        let mut call = token.transfer(dest, amt);
                        call.tx.set_gas(gas_limit_c);
                        call.tx.set_gas_price(gas_price); // **the fix**
                        call.tx.set_nonce(nonce);
                        nonce += U256::one();

                        let sig = signer
                            .signer()
                            .sign_transaction(&call.tx)
                            .await
                            .expect("sign");
                        let raw = call.tx.rlp_signed(&sig);

                        batch_raw.push(raw);
                        batch_permit.push(permit);
                        send_start.push(Instant::now());
                    }

                    if batch_raw.is_empty() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    //----------------------------------------------//
                    // 2. send JSON‑RPC batch                       //
                    //----------------------------------------------//
                    let payload: Vec<_> = batch_raw
                        .iter()
                        .enumerate()
                        .map(|(i, raw)| {
                            json!({
                                "jsonrpc": "2.0",
                                "id":      i,
                                "method":  "eth_sendRawTransaction",
                                "params":  [format!("0x{}", hex_encode(raw))]
                            })
                        })
                        .collect();

                    let resp = match http_c.post(&rpc_url_c).json(&payload).send().await {
                        Ok(r)  => r,
                        Err(e) => {
                            eprintln!("❗ batch send error {e}");
                            for p in batch_permit { drop(p); }
                            continue;
                        }
                    };

                    let replies: Vec<serde_json::Value> = match resp.json().await {
                        Ok(v)  => v,
                        Err(e) => {
                            eprintln!("❗ bad JSON reply {e}");
                            for p in batch_permit { drop(p); }
                            continue;
                        }
                    };

                    //----------------------------------------------//
                    // 3. per‑tx accounting & receipt waiters       //
                    //----------------------------------------------//
                    for ((permit, t_start), reply) in batch_permit
                        .into_iter()
                        .zip(send_start.into_iter())
                        .zip(replies.into_iter())
                    {
                        let sub_ms = t_start.elapsed().as_millis() as u64;

                        if let Some(tx_hash_str) = reply.get("result").and_then(|v| v.as_str()) {
                            // success
                            let tx_hash: H256 = tx_hash_str.parse().unwrap_or_default();
                            m.submit.write().record(sub_ms).ok();
                            m.sub_last.lock().push_back((Instant::now(), sub_ms));
                            m.sent.fetch_add(1, Ordering::Relaxed);

                            let prov  = provider_c.clone();
                            let m_inc = m.clone();
                            tokio::spawn(async move {
                                let t_inc = Instant::now();
                                loop {
                                    match prov.get_transaction_receipt(tx_hash).await {
                                        Ok(Some(_)) => {
                                            let inc = t_inc.elapsed().as_millis() as u64;
                                            m_inc.include.write().record(inc).ok();
                                            m_inc
                                                .inc_last
                                                .lock()
                                                .push_back((Instant::now(), inc));
                                            m_inc.included.fetch_add(1, Ordering::Relaxed);
                                            break;
                                        }
                                        Ok(None) => {
                                            tokio::time::sleep(Duration::from_millis(100)).await
                                        }
                                        Err(_) => break,
                                    }
                                }
                                drop(permit); // free slot
                            });
                        } else {
                            if let Some(err) = reply.get("error") {
                                eprintln!("❗ tx error {err}");
                            }
                            drop(permit);
                        }
                    }
                }
            })
        })
        .collect()
}
