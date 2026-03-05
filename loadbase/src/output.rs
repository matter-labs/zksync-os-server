use std::{cmp::Ordering, fs, path::Path, time::Duration};

use anyhow::Context;
use serde_json::json;

#[derive(Clone, Debug, PartialEq)]
pub struct BenchmarkSample {
    pub elapsed_s: u64,
    pub sent: u64,
    pub in_flight: u64,
    pub included: u64,
    pub tps10: f64,
    pub tps_avg: f64,
    pub submit_p50_10s_ms: u64,
    pub submit_p50_total_ms: u64,
    pub include_p50_10s_s: f64,
    pub include_p50_total_s: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BenchmarkSummary {
    pub sample_count: usize,
    pub duration: Duration,
    pub median_tps10: f64,
    pub p95_tps10: f64,
    pub median_include_p50_10s_s: f64,
    pub max_in_flight: u64,
    pub final_included: u64,
}

#[derive(Clone, Debug)]
pub struct RunMetadata {
    pub chain_id: u64,
    pub wallets: u32,
    pub max_in_flight: u32,
    pub duration_s: u64,
    pub destination_mode: String,
    pub rpc_url: String,
    pub receipt_timeouts: u64,
    pub receipt_errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
    Csv,
    All,
}

impl OutputMode {
    pub fn writes_json(self) -> bool {
        matches!(self, Self::Json | Self::All)
    }

    pub fn writes_csv(self) -> bool {
        matches!(self, Self::Csv | Self::All)
    }
}

pub fn render_json(_samples: &[BenchmarkSample]) -> String {
    let payload: Vec<_> = _samples
        .iter()
        .map(|sample| {
            json!({
                "elapsed_s": sample.elapsed_s,
                "sent": sample.sent,
                "in_flight": sample.in_flight,
                "included": sample.included,
                "tps10": sample.tps10,
                "tps_avg": sample.tps_avg,
                "submit_p50_10s_ms": sample.submit_p50_10s_ms,
                "submit_p50_total_ms": sample.submit_p50_total_ms,
                "include_p50_10s_s": sample.include_p50_10s_s,
                "include_p50_total_s": sample.include_p50_total_s,
            })
        })
        .collect();
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_owned())
}

pub fn render_csv(_samples: &[BenchmarkSample]) -> String {
    let mut out = String::from(
        "elapsed_s,sent,in_flight,included,tps10,tps_avg,submit_p50_10s_ms,submit_p50_total_ms,include_p50_10s_s,include_p50_total_s\n",
    );

    for sample in _samples {
        let line = format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            sample.elapsed_s,
            sample.sent,
            sample.in_flight,
            sample.included,
            sample.tps10,
            sample.tps_avg,
            sample.submit_p50_10s_ms,
            sample.submit_p50_total_ms,
            sample.include_p50_10s_s,
            sample.include_p50_total_s
        );
        out.push_str(&line);
    }
    out
}

pub fn build_summary(_samples: &[BenchmarkSample]) -> BenchmarkSummary {
    if _samples.is_empty() {
        return BenchmarkSummary {
            sample_count: 0,
            duration: Duration::from_secs(0),
            median_tps10: 0.0,
            p95_tps10: 0.0,
            median_include_p50_10s_s: 0.0,
            max_in_flight: 0,
            final_included: 0,
        };
    }

    let duration = Duration::from_secs(_samples.last().map_or(0, |s| s.elapsed_s));
    let median_tps10 = percentile(_samples.iter().map(|s| s.tps10).collect(), 0.5);
    let p95_tps10 = percentile(_samples.iter().map(|s| s.tps10).collect(), 0.95);
    let median_include_p50_10s_s =
        percentile(_samples.iter().map(|s| s.include_p50_10s_s).collect(), 0.5);
    let max_in_flight = _samples.iter().map(|s| s.in_flight).max().unwrap_or(0);
    let final_included = _samples.last().map_or(0, |s| s.included);

    BenchmarkSummary {
        sample_count: _samples.len(),
        duration,
        median_tps10,
        p95_tps10,
        median_include_p50_10s_s,
        max_in_flight,
        final_included,
    }
}

fn percentile(mut values: Vec<f64>, q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let idx = ((values.len() as f64 * q).ceil() as usize).saturating_sub(1);
    values[idx.min(values.len() - 1)]
}

pub fn write_outputs(
    output_dir: &Path,
    mode: OutputMode,
    metadata: &RunMetadata,
    samples: &[BenchmarkSample],
) -> anyhow::Result<()> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("failed creating output dir {}", output_dir.display()))?;

    let summary = build_summary(samples);
    let summary_json = render_summary_json(metadata, &summary);
    fs::write(output_dir.join("summary.json"), summary_json).with_context(|| {
        format!(
            "failed writing {}",
            output_dir.join("summary.json").display()
        )
    })?;

    let report_md = render_report_md(metadata, &summary);
    fs::write(output_dir.join("report.md"), report_md)
        .with_context(|| format!("failed writing {}", output_dir.join("report.md").display()))?;

    if mode.writes_json() {
        fs::write(output_dir.join("metrics.json"), render_json(samples)).with_context(|| {
            format!(
                "failed writing {}",
                output_dir.join("metrics.json").display()
            )
        })?;
    }
    if mode.writes_csv() {
        fs::write(output_dir.join("metrics.csv"), render_csv(samples)).with_context(|| {
            format!(
                "failed writing {}",
                output_dir.join("metrics.csv").display()
            )
        })?;
    }
    Ok(())
}

fn render_summary_json(metadata: &RunMetadata, summary: &BenchmarkSummary) -> String {
    let payload = json!({
        "schema_version": 1,
        "metadata": {
            "chain_id": metadata.chain_id,
            "wallets": metadata.wallets,
            "max_in_flight": metadata.max_in_flight,
            "duration_s": metadata.duration_s,
            "destination_mode": metadata.destination_mode,
            "rpc_url": metadata.rpc_url,
        },
        "summary": {
            "sample_count": summary.sample_count,
            "duration_s": summary.duration.as_secs(),
            "median_tps10": summary.median_tps10,
            "p95_tps10": summary.p95_tps10,
            "median_include_p50_10s_s": summary.median_include_p50_10s_s,
            "max_in_flight": summary.max_in_flight,
            "final_included": summary.final_included,
            "receipt_timeouts": metadata.receipt_timeouts,
            "receipt_errors": metadata.receipt_errors,
        }
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
}

fn render_report_md(metadata: &RunMetadata, summary: &BenchmarkSummary) -> String {
    format!(
        "# Loadbase Benchmark Report\n\n\
        | Metric | Value |\n\
        |---|---:|\n\
        | Chain ID | {} |\n\
        | RPC URL | `{}` |\n\
        | Wallets | {} |\n\
        | Max in-flight | {} |\n\
        | Duration (target s) | {} |\n\
        | Destination mode | {} |\n\
        | Samples | {} |\n\
        | Duration (observed s) | {} |\n\
        | Median TPS10 | {:.2} |\n\
        | P95 TPS10 | {:.2} |\n\
        | Median include p50 10s (s) | {:.3} |\n\
        | Max in-flight observed | {} |\n\
        | Final included | {} |\n",
        metadata.chain_id,
        metadata.rpc_url,
        metadata.wallets,
        metadata.max_in_flight,
        metadata.duration_s,
        metadata.destination_mode,
        summary.sample_count,
        summary.duration.as_secs(),
        summary.median_tps10,
        summary.p95_tps10,
        summary.median_include_p50_10s_s,
        summary.max_in_flight,
        summary.final_included,
    ) + &format!(
        "| Receipt timeouts | {} |\n\
         | Receipt errors | {} |\n",
        metadata.receipt_timeouts, metadata.receipt_errors
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        elapsed_s: u64,
        tps10: f64,
        in_flight: u64,
        included: u64,
        inc_p50: f64,
    ) -> BenchmarkSample {
        BenchmarkSample {
            elapsed_s,
            sent: included + in_flight,
            in_flight,
            included,
            tps10,
            tps_avg: tps10,
            submit_p50_10s_ms: 3,
            submit_p50_total_ms: 4,
            include_p50_10s_s: inc_p50,
            include_p50_total_s: inc_p50,
        }
    }

    #[test]
    fn renders_samples_to_json() {
        let samples = vec![sample(1, 10.0, 2, 8, 0.2)];
        let rendered = render_json(&samples);
        assert!(rendered.contains("\"elapsed_s\": 1"));
        assert!(rendered.contains("\"tps10\": 10.0"));
    }

    #[test]
    fn renders_samples_to_csv() {
        let samples = vec![sample(2, 11.5, 4, 20, 0.25)];
        let rendered = render_csv(&samples);
        assert!(rendered.starts_with("elapsed_s,sent,in_flight,included,tps10,tps_avg"));
        assert!(rendered.contains("\n2,24,4,20,11.5,11.5,3,4,0.25,0.25\n"));
    }

    #[test]
    fn computes_summary_percentiles() {
        let samples = vec![
            sample(1, 10.0, 3, 8, 0.2),
            sample(2, 20.0, 5, 30, 0.3),
            sample(3, 30.0, 4, 50, 0.4),
        ];
        let summary = build_summary(&samples);
        assert_eq!(summary.sample_count, 3);
        assert_eq!(summary.duration, Duration::from_secs(3));
        assert_eq!(summary.median_tps10, 20.0);
        assert_eq!(summary.p95_tps10, 30.0);
        assert_eq!(summary.median_include_p50_10s_s, 0.3);
        assert_eq!(summary.max_in_flight, 5);
        assert_eq!(summary.final_included, 50);
    }

    #[test]
    fn renders_receipt_outcomes_in_summary_json() {
        let metadata = RunMetadata {
            chain_id: 6565,
            wallets: 30,
            max_in_flight: 25,
            duration_s: 300,
            destination_mode: "random".to_owned(),
            rpc_url: "http://localhost:3050".to_owned(),
            receipt_timeouts: 7,
            receipt_errors: 2,
        };
        let summary = BenchmarkSummary {
            sample_count: 1,
            duration: Duration::from_secs(1),
            median_tps10: 10.0,
            p95_tps10: 12.0,
            median_include_p50_10s_s: 0.5,
            max_in_flight: 20,
            final_included: 100,
        };
        let payload = render_summary_json(&metadata, &summary);
        assert!(payload.contains("\"receipt_timeouts\": 7"));
        assert!(payload.contains("\"receipt_errors\": 2"));
    }
}
