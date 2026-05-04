//! Tokio runtime health metrics.
//!
//! Exposes worker thread saturation, queue depths, and blocking pool usage to Prometheus.
//! These metrics are the primary signal for diagnosing worker thread starvation — where
//! synchronous code running on async worker threads starves other tasks without necessarily
//! saturating CPU (e.g. blocking storage reads block the thread but do no compute).

use std::sync::Mutex;

use tokio::runtime::Handle;
use tokio_metrics::RuntimeMonitor;
use vise::{Collector, Gauge, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "tokio_runtime")]
struct TokioRuntimeMetrics {
    /// Ratio of time worker threads spent busy vs total elapsed, summed across all workers.
    /// Values approaching `workers_count` indicate full saturation.
    worker_busy_ratio: Gauge<f64>,
    /// Number of worker threads in the runtime.
    workers_count: Gauge<u64>,
    /// Tasks currently in the runtime's global queue.
    global_queue_depth: Gauge<u64>,
    /// Total live tasks in the runtime.
    live_tasks_count: Gauge<u64>,
    /// Total tasks waiting across all worker-local queues.
    total_local_queue_depth: Gauge<u64>,
    /// Tasks queued for `spawn_blocking`, waiting for a thread.
    blocking_queue_depth: Gauge<u64>,
    /// Threads currently spawned by the runtime for `spawn_blocking`.
    blocking_threads_count: Gauge<u64>,
    /// Idle `spawn_blocking` threads available for reuse.
    idle_blocking_threads_count: Gauge<u64>,
    /// Mean task poll duration during the last interval.
    #[metrics(unit = Unit::Seconds)]
    mean_poll_duration: Gauge<f64>,
}

#[vise::register]
static METRICS: Collector<TokioRuntimeMetrics> = Collector::new();

/// Registers a Prometheus collector that samples Tokio runtime metrics on each scrape.
///
/// The sampling window exactly matches the scrape interval — no hardcoded sleep needed.
/// Must be called from within a Tokio runtime context.
pub fn register_monitor() {
    let handle = Handle::current();
    let intervals = Mutex::new(RuntimeMonitor::new(&handle).intervals());

    METRICS
        .before_scrape(move || {
            let interval = intervals.lock().unwrap().next().expect("infinite iterator");
            let m = TokioRuntimeMetrics::default();
            m.worker_busy_ratio.set(interval.busy_ratio());
            m.workers_count.set(interval.workers_count as u64);
            m.global_queue_depth.set(interval.global_queue_depth as u64);
            m.live_tasks_count.set(interval.live_tasks_count as u64);
            m.total_local_queue_depth
                .set(interval.total_local_queue_depth as u64);
            m.blocking_queue_depth
                .set(interval.blocking_queue_depth as u64);
            m.blocking_threads_count
                .set(interval.blocking_threads_count as u64);
            m.idle_blocking_threads_count
                .set(interval.idle_blocking_threads_count as u64);
            m.mean_poll_duration
                .set(interval.mean_poll_duration.as_secs_f64());
            m
        })
        .ok();
}
