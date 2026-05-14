use std::sync::Once;

use anyhow::{Result, anyhow};
use tikv_jemalloc_ctl::{epoch, stats};
use vise::{Collector, Gauge, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "jemalloc")]
struct JemallocMetrics {
    /// Live application heap payload allocated through jemalloc.
    #[metrics(unit = Unit::Bytes)]
    allocated: Gauge<usize>,
    /// Jemalloc pages backing active allocations.
    #[metrics(unit = Unit::Bytes)]
    active: Gauge<usize>,
    /// Jemalloc memory currently resident in RAM.
    #[metrics(unit = Unit::Bytes)]
    resident: Gauge<usize>,
    /// Virtual memory mapped by jemalloc.
    #[metrics(unit = Unit::Bytes)]
    mapped: Gauge<usize>,
    /// Virtual memory retained by jemalloc for reuse.
    #[metrics(unit = Unit::Bytes)]
    retained: Gauge<usize>,
    /// Memory used by jemalloc metadata.
    #[metrics(unit = Unit::Bytes)]
    metadata: Gauge<usize>,
    /// Whether jemalloc stats were successfully collected during the scrape.
    stats_available: Gauge<u64>,
}

#[vise::register]
static METRICS: Collector<JemallocMetrics> = Collector::new();

static WARN_ON_COLLECT_ERROR: Once = Once::new();

pub fn register_monitor() {
    METRICS.before_scrape(scrape).ok();
}

fn scrape() -> JemallocMetrics {
    collect_stats()
        .inspect_err(|err| {
            WARN_ON_COLLECT_ERROR.call_once(|| {
                tracing::warn!(
                    %err,
                    "Failed collecting jemalloc stats; jemalloc metrics will be marked unavailable"
                );
            });
        })
        .unwrap_or_default()
}

fn read_ctl<T>(
    stat: &'static str,
    read: impl FnOnce() -> std::result::Result<T, tikv_jemalloc_ctl::Error>,
) -> Result<T> {
    read().map_err(|err| anyhow!("failed reading jemalloc `{stat}`: {err}"))
}

fn collect_stats() -> Result<JemallocMetrics> {
    read_ctl("epoch", epoch::advance)?;

    let metrics = JemallocMetrics::default();
    metrics
        .allocated
        .set(read_ctl("stats.allocated", stats::allocated::read)?);
    metrics
        .active
        .set(read_ctl("stats.active", stats::active::read)?);
    metrics
        .resident
        .set(read_ctl("stats.resident", stats::resident::read)?);
    metrics
        .mapped
        .set(read_ctl("stats.mapped", stats::mapped::read)?);
    metrics
        .retained
        .set(read_ctl("stats.retained", stats::retained::read)?);
    metrics
        .metadata
        .set(read_ctl("stats.metadata", stats::metadata::read)?);
    metrics.stats_available.set(1);

    Ok(metrics)
}
