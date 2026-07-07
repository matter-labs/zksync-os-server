//! The tick loop: one transaction per tick, sender picked round-robin,
//! workload picked by profile weight. The engine owns everything stateful —
//! nonces, fees, submission, error handling, counting — so a workload is only
//! ever asked one question: "what's the next payload?"

use super::bank::Bank;
use super::stats::{LedgerEntry, LoadStats};
use super::workloads::Workload;
use alloy::network::TransactionBuilder;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use rand08::{Rng as _, SeedableRng as _, rngs::StdRng};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fee refresh + wedged-sender rescue cadence.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);

pub struct EngineConfig {
    pub tps: u32,
    pub bursts: Option<(u64, u64)>,
    pub duration: Option<Duration>,
    pub key_seed: u64,
}

/// Runs the loop until the duration elapses or ctrl-c. Returns the elapsed
/// time; counting lives in `stats`.
pub async fn run(
    config: EngineConfig,
    bank: &mut Bank,
    workloads: &mut [(Box<dyn Workload>, u32)],
    stats: &Arc<Mutex<LoadStats>>,
) -> Duration {
    let started = Instant::now();
    let deadline = config.duration.map(|duration| started + duration);
    let tick = Duration::from_secs_f64(1.0 / f64::from(config.tps.max(1)));
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rng = StdRng::seed_from_u64(config.key_seed);
    let total_weight: u32 = workloads.iter().map(|(_, weight)| *weight).sum();
    let mut next_sender = 0usize;
    let mut maintained = Instant::now() - MAINTENANCE_INTERVAL;

    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break;
        }
        if let Some((burst_secs, idle_secs)) = config.bursts {
            let cycle = (burst_secs + idle_secs).max(1);
            if started.elapsed().as_secs() % cycle >= burst_secs {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        }
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => break,
        }

        if maintained.elapsed() > MAINTENANCE_INTERVAL {
            maintained = Instant::now();
            // Rescue resubmissions are ordinary traffic for the counters.
            for (sender_index, accepted) in bank.maintain().await {
                let validator = bank.senders[sender_index].validator;
                let mut stats = stats.lock().unwrap();
                if accepted {
                    stats.per_validator[validator].accepted += 1;
                } else {
                    stats.per_validator[validator].rejected += 1;
                }
            }
        }

        // Pick the workload by weight, then let it build the payload.
        let mut roll = rng.gen_range(0..total_weight);
        let mut picked = 0usize;
        for (index, (_, weight)) in workloads.iter().enumerate() {
            if roll < *weight {
                picked = index;
                break;
            }
            roll -= weight;
        }
        let (workload, _) = &mut workloads[picked];
        let plan = workload.fire(&mut rng);
        let name = workload.name();

        let (chain_id, fees) = (bank.chain_id, bank.fees);
        let sender_index = next_sender;
        next_sender = (next_sender + 1) % bank.senders.len();
        let sender = &mut bank.senders[sender_index];
        let request = TransactionRequest::default()
            .with_chain_id(chain_id)
            .with_from(sender.address)
            .with_to(plan.to)
            .with_value(plan.value)
            .with_input(plan.input)
            .with_nonce(sender.nonce)
            .with_gas_limit(plan.gas_limit)
            .with_max_fee_per_gas(fees.0)
            .with_max_priority_fee_per_gas(fees.1);
        match sender.provider.send_transaction(request).await {
            Ok(pending) => {
                let hash = *pending.tx_hash();
                sender.last_hash = Some(hash);
                sender.nonce += 1;
                stats.lock().unwrap().accepted(
                    LedgerEntry {
                        workload: name,
                        expect: plan.expect,
                        hash,
                        sender: sender_index,
                    },
                    sender.validator,
                );
            }
            Err(error) => {
                let text = error.to_string();
                stats
                    .lock()
                    .unwrap()
                    .rejected(name, sender.validator, &text);
                // Down validators refuse connections — routine under chaos. A
                // nonce drift (e.g. after an RPC error whose tx still landed)
                // resyncs from the chain.
                if text.contains("nonce")
                    && let Ok(fresh) = sender
                        .provider
                        .get_transaction_count(sender.address)
                        .pending()
                        .await
                {
                    sender.nonce = fresh;
                }
            }
        }
    }

    started.elapsed()
}

#[cfg(test)]
mod tests {
    /// The weighted pick, extracted: rolls in `0..total` land on workloads in
    /// proportion to their weights, and every weight is reachable.
    #[test]
    fn weighted_pick_covers_all_weights() {
        let weights = [40u32, 22, 10, 6, 6, 6, 4, 2];
        let total: u32 = weights.iter().sum();
        let mut hits = vec![0u32; weights.len()];
        for mut roll in 0..total {
            let mut picked = 0usize;
            for (index, weight) in weights.iter().enumerate() {
                if roll < *weight {
                    picked = index;
                    break;
                }
                roll -= weight;
            }
            hits[picked] += 1;
        }
        assert_eq!(hits, weights.to_vec());
    }
}
