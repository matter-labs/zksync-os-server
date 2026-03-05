// src/metrics.rs
//! Per-second TPS / latency reporter.

use hdrhistogram::Histogram;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::time::interval;

use crate::output::BenchmarkSample;

struct TxStats {
    count: u64,
    hist: Histogram<u64>,
    recent: VecDeque<(Instant, u64)>,
}

impl TxStats {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            count: 0,
            hist: Histogram::new_with_max(60_000, 3)?,
            recent: VecDeque::new(),
        })
    }

    fn record(&mut self, ms: u64) {
        self.count += 1;
        self.hist.record(ms).ok();
        self.recent.push_back((Instant::now(), ms));
    }

    fn p50_total(&self) -> u64 {
        self.hist.value_at_quantile(0.5)
    }

    fn p50_recent(&mut self, now: Instant, window: Duration) -> u64 {
        self.recent.retain(|(t, _)| *t + window >= now);
        let mut v: Vec<u64> = self.recent.iter().map(|(_, x)| *x).collect();
        v.sort_unstable();
        if v.is_empty() {
            0
        } else {
            v[v.len() / 2]
        }
    }
}

#[derive(Clone)]
pub struct Metrics {
    submit: Arc<Mutex<TxStats>>,
    include: Arc<Mutex<TxStats>>,
    samples: Arc<Mutex<Vec<BenchmarkSample>>>,
    receipt_timeouts: Arc<AtomicU64>,
    receipt_errors: Arc<AtomicU64>,
}

impl Metrics {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            submit: Arc::new(Mutex::new(TxStats::new()?)),
            include: Arc::new(Mutex::new(TxStats::new()?)),
            samples: Arc::new(Mutex::new(Vec::new())),
            receipt_timeouts: Arc::new(AtomicU64::new(0)),
            receipt_errors: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn record_submitted(&self, ms: u64) {
        self.submit.lock().record(ms);
    }

    pub fn record_included(&self, ms: u64) {
        self.include.lock().record(ms);
    }

    pub fn spawn_reporter(&self, started: Instant) {
        let me = self.clone();
        tokio::spawn(async move { me.report_loop(started).await });
    }

    pub fn record_receipt_timeout(&self) {
        self.receipt_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_receipt_error(&self) {
        self.receipt_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn receipt_outcomes(&self) -> (u64, u64) {
        (
            self.receipt_timeouts.load(Ordering::Relaxed),
            self.receipt_errors.load(Ordering::Relaxed),
        )
    }

    pub fn samples(&self) -> Vec<BenchmarkSample> {
        self.samples.lock().clone()
    }

    #[cfg(test)]
    pub fn manual_snapshot(
        &self,
        now: Instant,
        window: Duration,
        last_included: u64,
    ) -> BenchmarkSample {
        let started = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        let mut tps_q = VecDeque::new();
        let mut last = last_included;
        self.snapshot(now, started, window, &mut tps_q, &mut last)
    }

    fn snapshot(
        &self,
        now: Instant,
        started: Instant,
        window: Duration,
        tps_q: &mut VecDeque<(Instant, u64)>,
        last_included: &mut u64,
    ) -> BenchmarkSample {
        while tps_q.front().is_some_and(|(t, _)| *t + window < now) {
            tps_q.pop_front();
        }

        let (sent, sub_p50_tot, sub_p50_10) = {
            let mut s = self.submit.lock();
            (s.count, s.p50_total(), s.p50_recent(now, window))
        };
        let (inc_now, inc_p50_tot, inc_p50_10) = {
            let mut s = self.include.lock();
            (s.count, s.p50_total(), s.p50_recent(now, window))
        };

        let delta_inc = inc_now.saturating_sub(*last_included);
        *last_included = inc_now;
        tps_q.push_back((now, delta_inc));
        let tps10: u64 = tps_q.iter().map(|(_, d)| *d).sum();
        let tps_avg = if started.elapsed().is_zero() {
            0.0
        } else {
            inc_now as f64 / started.elapsed().as_secs_f64()
        };
        let in_flight = sent.saturating_sub(inc_now);

        BenchmarkSample {
            elapsed_s: started.elapsed().as_secs(),
            sent,
            in_flight,
            included: inc_now,
            tps10: tps10 as f64 / 10.0,
            tps_avg,
            submit_p50_10s_ms: sub_p50_10,
            submit_p50_total_ms: sub_p50_tot,
            include_p50_10s_s: inc_p50_10 as f64 / 1000.0,
            include_p50_total_s: inc_p50_tot as f64 / 1000.0,
        }
    }

    async fn report_loop(self, started: Instant) {
        let mut tick = interval(Duration::from_secs(1));
        let mut tps_q: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut last_inc = 0u64;
        let window = Duration::from_secs(10);

        loop {
            tick.tick().await;
            let now = Instant::now();

            let sample = self.snapshot(now, started, window, &mut tps_q, &mut last_inc);
            self.samples.lock().push(sample.clone());

            println!(
                "⏱ {:>4}s | sent {:>7} | in-fl {:>5} | incl {:>7} | TPS10 {:>6.1} \
                 | TPSavg {:>6.1} | sub p50 10s {:>3} ms / tot {:>3} \
                 | inc p50 10s {:>5.2} s / tot {:>5.2} s",
                sample.elapsed_s,
                sample.sent,
                sample.in_flight,
                sample.included,
                sample.tps10,
                sample.tps_avg,
                sample.submit_p50_10s_ms,
                sample.submit_p50_total_ms,
                sample.include_p50_10s_s,
                sample.include_p50_total_s,
            );
            std::io::stdout().flush().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_empty_samples() {
        let metrics = Metrics::new().expect("metrics");
        assert!(metrics.samples().is_empty());
    }

    #[test]
    fn captures_manual_snapshot() {
        let metrics = Metrics::new().expect("metrics");
        metrics.record_submitted(5);
        metrics.record_included(50);
        let snapshot = metrics.manual_snapshot(Instant::now(), Duration::from_secs(10), 0);
        assert_eq!(snapshot.sent, 1);
        assert_eq!(snapshot.included, 1);
        assert_eq!(snapshot.in_flight, 0);
    }

    #[test]
    fn tracks_receipt_outcomes() {
        let metrics = Metrics::new().expect("metrics");
        metrics.record_receipt_timeout();
        metrics.record_receipt_timeout();
        metrics.record_receipt_error();
        let (timeouts, errors) = metrics.receipt_outcomes();
        assert_eq!(timeouts, 2);
        assert_eq!(errors, 1);
    }
}
