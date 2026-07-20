use crate::config::TxGasRateLimitConfig;
use crate::metrics::TX_GAS_RATE_LIMITER;
use crate::rpc_storage::ReadRpcStorage;
use alloy::primitives::Address;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zksync_os_types::ZkEnvelope;

/// Rate limiter for incoming L2 transactions based on *executed* gas throughput.
///
/// A shared "gas bank" is drained by each sealed block's executed gas and refilled by
/// wall-clock time at `gas_per_second`. The gate closes when the bank is exhausted and
/// reopens once it recovers `reopen_credit`, so acceptance flips in windows of seconds
/// rather than per tx. Non-obvious properties:
/// - Declared `gas_limit` is never consulted: draining the bank requires getting txs
///   executed and paid for, so padding buys an attacker nothing.
/// - The bank goes negative down to `-deficit_floor`: overshoot is repaid before
///   reopening, keeping the long-run executed average at `gas_per_second`.
/// - The drain is block-granular; overshoot admitted within a block self-corrects via
///   the deficit. There is no per-transaction bookkeeping.
pub struct TxGasRateLimiter {
    /// Refill rate, gas per second.
    rate: f64,
    /// Bank capacity: idle burst headroom, gas.
    max_credit: f64,
    /// Hysteresis: bank level required to reopen the gate, gas.
    reopen_credit: f64,
    /// Lowest allowed bank level (`<= 0`): max remembered deficit, gas.
    floor: f64,
    exempt_senders: HashSet<Address>,
    bank: Mutex<Bank>,
}

struct Bank {
    level: f64,
    last_refill: Instant,
    gate_open: bool,
}

impl TxGasRateLimiter {
    pub fn new(config: &TxGasRateLimitConfig) -> Self {
        // Guards the retry-after division; node-level config makes 0 unrepresentable.
        assert!(
            config.gas_per_second > 0,
            "tx_gas_rate_limit.gas_per_second must be positive"
        );
        let rate = config.gas_per_second as f64;
        let limiter = Self {
            rate,
            max_credit: config.max_credit_seconds * rate,
            reopen_credit: config.reopen_credit_seconds * rate,
            floor: -(config.deficit_floor_seconds * rate),
            exempt_senders: config.exempt_senders.clone(),
            bank: Mutex::new(Bank {
                level: config.max_credit_seconds * rate,
                last_refill: Instant::now(),
                gate_open: true,
            }),
        };
        TX_GAS_RATE_LIMITER.gate_open.set(1);
        TX_GAS_RATE_LIMITER
            .bank_level_gas
            .set(limiter.max_credit as i64);
        limiter
    }

    pub fn is_exempt(&self, sender: &Address) -> bool {
        self.exempt_senders.contains(sender)
    }

    pub fn note_exempt_admission(&self) {
        let mut bank = self.bank.lock().unwrap();
        // Refresh first: after an idle stretch the gate may already be reopenable,
        // and counting against the stale state would overstate closed time.
        self.refill(&mut bank, Instant::now());
        self.update_gate(&mut bank);
        if !bank.gate_open {
            TX_GAS_RATE_LIMITER.exempt_admitted_while_closed.inc();
        }
    }

    /// On rejection returns a suggested retry delay: a lower bound until the gate can
    /// reopen, jittered upwards so synchronized clients don't stampede the reopen instant.
    pub fn try_admit(&self) -> Result<(), Duration> {
        self.try_admit_at(Instant::now(), rand::random::<f64>)
    }

    fn try_admit_at(&self, now: Instant, jitter: impl FnOnce() -> f64) -> Result<(), Duration> {
        let mut bank = self.bank.lock().unwrap();
        self.refill(&mut bank, now);
        self.update_gate(&mut bank);
        if bank.gate_open {
            Ok(())
        } else {
            let base_secs = ((self.reopen_credit - bank.level) / self.rate).max(0.0);
            // Capped at 1h: hints beyond that are useless, and the cap keeps
            // `from_secs_f64` panic-free for any accepted config.
            let secs = (base_secs * (1.0 + jitter() * 0.5)).min(3600.0);
            Err(Duration::from_secs_f64(secs))
        }
    }

    pub fn on_block(&self, block_gas_used: u64) {
        self.on_block_at(block_gas_used, Instant::now())
    }

    fn on_block_at(&self, block_gas_used: u64, now: Instant) {
        let mut bank = self.bank.lock().unwrap();
        self.refill(&mut bank, now);
        bank.level = (bank.level - block_gas_used as f64).max(self.floor);
        self.update_gate(&mut bank);
        // Block-granular is fresh enough for a scraped gauge; keeping the write out of
        // `try_admit` keeps the admission critical section minimal.
        TX_GAS_RATE_LIMITER.bank_level_gas.set(bank.level as i64);
    }

    fn refill(&self, bank: &mut Bank, now: Instant) {
        let elapsed = now.saturating_duration_since(bank.last_refill);
        bank.last_refill = now;
        bank.level = (bank.level + elapsed.as_secs_f64() * self.rate).min(self.max_credit);
    }

    fn update_gate(&self, bank: &mut Bank) {
        if bank.gate_open && bank.level <= 0.0 {
            bank.gate_open = false;
            tracing::warn!(
                bank_level_gas = bank.level as i64,
                reopen_credit_gas = self.reopen_credit as i64,
                "tx gas rate limiter: bank exhausted, suspending acceptance of non-exempt transactions"
            );
            TX_GAS_RATE_LIMITER.gate_closes.inc();
            TX_GAS_RATE_LIMITER.gate_open.set(0);
        } else if !bank.gate_open && bank.level >= self.reopen_credit {
            bank.gate_open = true;
            tracing::info!(
                bank_level_gas = bank.level as i64,
                "tx gas rate limiter: bank recovered, resuming acceptance"
            );
            TX_GAS_RATE_LIMITER.gate_open.set(1);
        }
    }
}

/// The broadcast channel behind the block stream holds 256 notifications, so a live
/// consumer can never miss more than that; larger gaps can only come from prolonged
/// stalls, where a repository fetch storm would hurt more than the lost drain.
const MAX_BACKFILL_BLOCKS: u64 = 256;

/// Drains the gas bank from the block stream and emits per-tx padding metrics.
/// If the stream ends (shutdown), parks forever so the select exits via the shutdown arm.
pub async fn run_drain<RpcStorage: ReadRpcStorage>(
    limiter: Arc<TxGasRateLimiter>,
    storage: RpcStorage,
) {
    // The bank starts full when the limiter starts, so only blocks sealed from this
    // moment on may drain it. Older blocks — WAL replay after a restart, external-node
    // catch-up — were produced before this bank existed and arrive at replay speed;
    // draining them would double-charge the bank and pin the gate closed until synced.
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut blocks = storage.block_subscriptions().block_stream();
    let mut next_expected: Option<u64> = None;
    while let Some(notification) = blocks.next().await {
        let number = notification.block.header.number;
        // The broadcast stream silently skips notifications when this consumer lags;
        // backfill from the repository so missed blocks still drain the bank
        // (otherwise the limiter fails open exactly when the node is busiest).
        if let Some(expected) = next_expected
            && expected < number
        {
            if number - expected > MAX_BACKFILL_BLOCKS {
                tracing::warn!(
                    from = expected,
                    to = number,
                    "tx gas rate limiter: block gap too large to backfill, skipping"
                );
            } else {
                for missed in expected..number {
                    match storage.repository().get_block_by_number(missed) {
                        Ok(Some(block)) => drain_block(
                            &limiter,
                            started_at,
                            block.header.timestamp,
                            block.header.gas_used,
                        ),
                        Ok(None) => {}
                        Err(err) => tracing::warn!(
                            %err,
                            block = missed,
                            "tx gas rate limiter: failed to backfill skipped block"
                        ),
                    }
                }
            }
        }
        next_expected = Some(number + 1);

        drain_block(
            &limiter,
            started_at,
            notification.block.header.timestamp,
            notification.block.header.gas_used,
        );
        for stored in notification.transactions.values() {
            // L1/upgrade/system txs have protocol-assigned gas limits; padding is
            // only meaningful for user txs.
            if matches!(stored.tx.envelope(), ZkEnvelope::L2(_)) {
                TX_GAS_RATE_LIMITER
                    .gas_padding
                    .observe(stored.tx.gas_limit().saturating_sub(stored.meta.gas_used));
            }
        }
    }
    tracing::warn!("block stream ended; tx gas rate limiter will not drain anymore");
    std::future::pending::<()>().await
}

fn drain_block(limiter: &TxGasRateLimiter, started_at: u64, block_timestamp: u64, gas_used: u64) {
    if block_timestamp < started_at {
        return;
    }
    limiter.on_block(gas_used);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> TxGasRateLimiter {
        // rate 100k gas/s, max credit 200k, reopen at 100k, floor at -200k
        TxGasRateLimiter::new(&TxGasRateLimitConfig {
            gas_per_second: 100_000,
            max_credit_seconds: 2.0,
            reopen_credit_seconds: 1.0,
            deficit_floor_seconds: 2.0,
            exempt_senders: HashSet::from([Address::repeat_byte(0xaa)]),
        })
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn starts_open_with_full_credit() {
        let l = limiter();
        let t0 = Instant::now();
        assert!(l.try_admit_at(t0, || 0.0).is_ok());
        // Draining just under the full credit keeps the gate open.
        l.on_block_at(199_999, t0);
        assert!(l.try_admit_at(t0, || 0.0).is_ok());
    }

    #[test]
    fn closes_when_bank_exhausted_and_reports_retry_after() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        // level = 0 → closed; recovery to reopen_credit (100k) takes 1s at 100k/s.
        let retry = l.try_admit_at(t0, || 0.0).unwrap_err();
        assert_eq!(retry, secs(1.0));
    }

    #[test]
    fn hysteresis_keeps_gate_closed_until_reopen_credit() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        assert!(l.try_admit_at(t0, || 0.0).is_err());
        // 0.99s later the bank is at 99k, just below the 100k reopen threshold.
        assert!(l.try_admit_at(t0 + secs(0.99), || 0.0).is_err());
        assert!(l.try_admit_at(t0 + secs(1.0), || 0.0).is_ok());
        // Once open, it stays open even though the level is below reopen_credit.
        l.on_block_at(50_000, t0 + secs(1.0));
        assert!(l.try_admit_at(t0 + secs(1.0), || 0.0).is_ok());
    }

    #[test]
    fn deficit_is_remembered_down_to_floor_and_repaid() {
        let l = limiter();
        let t0 = Instant::now();
        // Massive overshoot: bank clamps at the floor (-200k), not below.
        l.on_block_at(10_000_000, t0);
        // Recovery from -200k to +100k takes 3s at 100k/s.
        let retry = l.try_admit_at(t0, || 0.0).unwrap_err();
        assert_eq!(retry, secs(3.0));
        assert!(l.try_admit_at(t0 + secs(2.99), || 0.0).is_err());
        assert!(l.try_admit_at(t0 + secs(3.0), || 0.0).is_ok());
    }

    #[test]
    fn zero_floor_clamps_bank_at_zero() {
        let l = TxGasRateLimiter::new(&TxGasRateLimitConfig {
            gas_per_second: 100_000,
            max_credit_seconds: 2.0,
            reopen_credit_seconds: 1.0,
            deficit_floor_seconds: 0.0,
            exempt_senders: HashSet::new(),
        });
        let t0 = Instant::now();
        l.on_block_at(10_000_000, t0);
        // No deficit remembered: recovery is reopen_credit / rate regardless of overshoot.
        assert_eq!(l.try_admit_at(t0, || 0.0).unwrap_err(), secs(1.0));
    }

    #[test]
    fn refill_caps_at_max_credit() {
        let l = limiter();
        let t0 = Instant::now();
        // A long idle period must not accumulate more than max_credit (200k):
        // draining exactly max_credit afterwards empties the bank and closes the gate.
        l.on_block_at(0, t0 + secs(100.0));
        l.on_block_at(200_000, t0 + secs(100.0));
        assert!(l.try_admit_at(t0 + secs(100.0), || 0.0).is_err());
    }

    #[test]
    fn retry_after_is_jittered_upwards() {
        let l = limiter();
        let t0 = Instant::now();
        l.on_block_at(200_000, t0);
        // Base retry is 1s; jitter=1.0 stretches it by 50%.
        assert_eq!(l.try_admit_at(t0, || 1.0).unwrap_err(), secs(1.5));
    }

    #[test]
    fn exempt_senders_are_recognized() {
        let l = limiter();
        assert!(l.is_exempt(&Address::repeat_byte(0xaa)));
        assert!(!l.is_exempt(&Address::repeat_byte(0xbb)));
    }
}
