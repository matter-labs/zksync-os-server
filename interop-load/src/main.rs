mod config;
mod events;
mod interop;
mod json_rpc;
mod preflight;
mod setup;
mod summary;

use anyhow::{Context, Error};
use clap::{Parser, error::ErrorKind};
use serde_json::json;
use std::fs::File;
use std::io::BufWriter;
use uuid::Uuid;

use crate::config::{Args, Config};
use crate::events::EventWriter;
use crate::summary::{LatencyReport, Summary, Totals};

const HARNESS_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) => {
            let kind = err.kind();
            let _ = err.print();
            let code = if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                0
            } else {
                3
            };
            std::process::exit(code);
        }
    };

    let code = match run(args).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("interop-load failed: {:?}", err.source);
            err.exit_code
        }
    };
    std::process::exit(code);
}

struct AppError {
    exit_code: i32,
    source: Error,
}

impl AppError {
    fn config(error: Error) -> Self {
        Self {
            exit_code: 3,
            source: error,
        }
    }

    fn preflight(error: Error) -> Self {
        Self {
            exit_code: 1,
            source: error,
        }
    }

    fn runtime(error: Error) -> Self {
        Self {
            exit_code: 2,
            source: error,
        }
    }
}

async fn run(args: Args) -> Result<(), AppError> {
    let config = Config::from_args(args).map_err(AppError::config)?;
    std::fs::create_dir_all(&config.output_dir)
        .with_context(|| format!("failed to create {}", config.output_dir.display()))
        .map_err(AppError::runtime)?;

    write_json_file(config.output_dir.join("config.json"), &config).map_err(AppError::runtime)?;

    let run_id = Uuid::new_v4();
    let mut events = EventWriter::create(&config.output_dir, run_id).map_err(AppError::runtime)?;
    events
        .emit(
            "run_started",
            json!({
                "config": config,
                "scaffold_only": false,
                "git_sha": option_env!("GIT_SHA").unwrap_or("unknown"),
                "harness_version": HARNESS_VERSION,
            }),
        )
        .map_err(AppError::runtime)?;

    let preflight = match preflight::run(&config).await {
        Ok(report) => report,
        Err(err) => {
            let _ = events.emit(
                "run_aborted",
                json!({
                    "reason_class": "preflight_failed",
                    "reason_detail": err.to_string(),
                }),
            );
            let _ = events.flush();
            return Err(AppError::preflight(err.context("preflight failed")));
        }
    };
    write_json_file(config.output_dir.join("preflight.json"), &preflight)
        .map_err(AppError::runtime)?;
    if preflight.smoke_test_skipped {
        events
            .emit(
                "smoke_test_skipped",
                json!({"reason": preflight.smoke_test_skip_reason}),
            )
            .map_err(AppError::runtime)?;
    }
    events
        .emit(
            "preflight_passed",
            json!({
                "chain_a_id": preflight.chain_a_id,
                "chain_b_id": preflight.chain_b_id,
                "source_chain_ids": preflight.source_chain_ids,
                "destination_chain_ids": preflight.destination_chain_ids,
                "gateway_chain_id": preflight.gateway_chain_id,
                "l1_chain_id": preflight.l1_chain_id,
                "smoke_test_skipped": preflight.smoke_test_skipped,
                "metrics_enabled": preflight.metrics_enabled,
                "scaffold_only": false,
            }),
        )
        .map_err(AppError::runtime)?;

    let setup = setup::load(&config.setup).map_err(AppError::config)?;
    write_json_file(config.output_dir.join("setup.json"), &setup).map_err(AppError::runtime)?;

    let stats = if config.pubdata_probe {
        interop::run_pubdata_probe(&config, &mut events, &setup)
            .await
            .map_err(AppError::runtime)?
    } else {
        interop::run(&config, &mut events, &setup)
            .await
            .map_err(AppError::runtime)?
    };

    // Reconciliation is a no-op for the current source→propagation harness
    // (no executeBundle to verify state for). Spec §10 requires the file, so
    // we write a stub that future modes can extend.
    write_json_file(
        config.output_dir.join("reconciliation.json"),
        &serde_json::json!({
            "mode": if config.pubdata_probe { "pubdata_probe" } else { "source_propagation_only" },
            "reconciled_bundles": 0,
            "note": "no executeBundle in this harness; nothing to reconcile",
        }),
    )
    .map_err(AppError::runtime)?;

    let measured_duration_ms = config.duration_ms.saturating_sub(config.warmup_ms);
    let measured_duration_secs = (measured_duration_ms as f64 / 1000.0).max(f64::EPSILON);
    let latency = LatencyReport::from_samples(&stats.latency_samples);
    let summary = Summary {
        scaffold_only: false,
        open_loop_violated: stats.open_loop_violated,
        measured_duration_ms,
        source_submitted_per_sec: stats.source_submitted as f64 / measured_duration_secs,
        root_imported_per_sec: stats.root_imported as f64 / measured_duration_secs,
        final_backlog: stats.final_backlog,
        totals: Totals {
            source_submitted: stats.source_submitted,
            source_included: stats.source_included,
            proof_available: stats.proof_available,
            root_imported: stats.root_imported,
            failed_classified: stats.failed_classified,
            erc20_submitted: stats.erc20_submitted,
            base_submitted: stats.base_submitted,
            message_submitted: stats.message_submitted,
            ..Totals::default()
        },
        latency,
    };
    write_json_file(config.output_dir.join("summary.json"), &summary).map_err(AppError::runtime)?;

    // Headline latency to stderr so a run is legible without opening summary.json.
    match &summary.latency {
        Some(report) => {
            let e2e = &report.aggregate.end_to_end;
            eprintln!(
                "interop-load: source→destination latency (n={}): \
                 p50={}ms p90={}ms p95={}ms p99={}ms max={}ms",
                e2e.count, e2e.p50_ms, e2e.p90_ms, e2e.p95_ms, e2e.p99_ms, e2e.max_ms,
            );
        }
        None => eprintln!(
            "interop-load: no measured bundle reached the destination chain; \
             no latency to report"
        ),
    }

    events
        .emit(
            "run_completed",
            json!({
                "duration_ms": config.duration_ms,
                "measured_duration_ms": summary.measured_duration_ms,
                "source_submitted_per_sec": summary.source_submitted_per_sec,
                "root_imported_per_sec": summary.root_imported_per_sec,
                "open_loop_violated": summary.open_loop_violated,
                "totals": summary.totals,
                "final_backlog": summary.final_backlog,
                "latency": summary.latency,
                "scaffold_only": false,
            }),
        )
        .map_err(AppError::runtime)?;
    events.flush().map_err(AppError::runtime)?;

    Ok(())
}

fn write_json_file(
    path: impl AsRef<std::path::Path>,
    value: &impl serde::Serialize,
) -> anyhow::Result<()> {
    let path = path.as_ref();
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::config::Args;

    #[test]
    fn clap_marks_required_arguments_missing() {
        let err = Args::try_parse_from(["interop-load"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
