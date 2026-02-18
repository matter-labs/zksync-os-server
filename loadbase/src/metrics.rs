// src/metrics.rs
//! Per-second TPS / latency reporter.

use hdrhistogram::Histogram;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::interval;

struct TxStats {
    count:  u64,
    hist:   Histogram<u64>,
    recent: VecDeque<(Instant, u64)>,
}

impl TxStats {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            count:  0,
            hist:   Histogram::new_with_max(60_000, 3)?,
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
        if v.is_empty() { 0 } else { v[v.len() / 2] }
    }
}

#[derive(Clone)]
pub struct Metrics {
    submit:  Arc<Mutex<TxStats>>,
    include: Arc<Mutex<TxStats>>,
}

impl Metrics {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            submit:  Arc::new(Mutex::new(TxStats::new()?)),
            include: Arc::new(Mutex::new(TxStats::new()?)),
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

    async fn report_loop(self, started: Instant) {
        let mut tick = interval(Duration::from_secs(1));
        let mut tps_q: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut last_inc = 0u64;
        let window = Duration::from_secs(10);

        loop {
            tick.tick().await;
            let now = Instant::now();

            while tps_q.front().map_or(false, |(t, _)| *t + window < now) {
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

            let delta_inc = inc_now - last_inc;
            last_inc = inc_now;
            tps_q.push_back((now, delta_inc));
            let tps10: u64 = tps_q.iter().map(|(_, d)| *d).sum();
            let tps_avg = inc_now as f64 / started.elapsed().as_secs_f64();
            let in_flight = sent.saturating_sub(inc_now);

            println!(
                "⏱ {:>4}s | sent {:>7} | in-fl {:>5} | incl {:>7} | TPS10 {:>6.1} \
                 | TPSavg {:>6.1} | sub p50 10s {:>3} ms / tot {:>3} \
                 | inc p50 10s {:>5.2} s / tot {:>5.2} s",
                started.elapsed().as_secs(),
                sent,
                in_flight,
                inc_now,
                tps10 as f64 / 10.0,
                tps_avg,
                sub_p50_10,
                sub_p50_tot,
                inc_p50_10 as f64 / 1000.0,
                inc_p50_tot as f64 / 1000.0,
            );
            std::io::stdout().flush().ok();
        }
    }
}
