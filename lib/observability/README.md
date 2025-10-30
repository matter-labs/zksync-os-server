# zksync_os_observability

Utilities for wiring the zkSync OS observability stack together: structured logging, traces, metrics, Sentry alerts,
and helpers for measuring component health. The crate exposes a single entry
point (`ObservabilityBuilder`) plus opt-in modules you can mix and match inside binaries across the workspace.

## Highlights

- `ObservabilityBuilder` sets up tracing subscribers, OpenTelemetry exporters, and Sentry in one place.
- `Logs` configures terminal, JSON, or logfmt output, installs a panic hook, and respects `RUST_LOG`.
- `OpenTelemetry` builds OTLP layers for traces and logs, with sensible Kubernetes-aware defaults.
- `PrometheusExporterConfig` spins up HTTP endpoints or push-gateway jobs backed by `vise` metrics.
- `Sentry` integration filters WARN / ERROR events and flushes cleanly on shutdown.
- `ComponentStateReporter`, `GenericComponentState`, and `LatencyDistributionTracker` help record per-component states
  and pipeline timings.

## Getting Started

The crate is part of the workspace; add it to a binary target with the workspace alias:

```toml
[dependencies]
zksync_os_observability = { workspace = true }
```

### Minimal bootstrap

```rust
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_observability::{
    logs::{LogFormat, Logs},
    opentelemetry::{OpenTelemetry, OpenTelemetryLevel},
    ObservabilityBuilder, prometheus::PrometheusExporterConfig, sentry::Sentry,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let logs = Logs::new(LogFormat::Terminal, true)
        .with_log_directives(Some("my_binary=debug".to_owned()));

    let otlp = OpenTelemetry::new(
        OpenTelemetryLevel::INFO,
        std::env::var("OTLP_TRACES_URL").ok(),
        std::env::var("OTLP_LOGS_URL").ok(),
    )?;

    let sentry = std::env::var("SENTRY_DSN")
        .ok()
        .map(|dsn| Sentry::new(&dsn).unwrap().with_environment(std::env::var("ENV").ok()));

    let _guard = ObservabilityBuilder::new()
        .with_logs(Some(logs))
        .with_opentelemetry(Some(otlp))
        .with_sentry(sentry)
        .build();

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        PrometheusExporterConfig::pull(9101)
            .run(stop_rx)
            .await
            .expect("metrics exporter");
    });

    // ...rest of your application...
    drop(stop_tx); // signal metrics task to stop
    Ok(())
}
```

Dropping the ObservabilityGuard returned by build flushes telemetry providers and closes the Sentry client, so keep it alive
for as long as you need instrumentation.

## Logging

- Choose between LogFormat::Terminal, LogFormat::Json, and LogFormat::LogFmt.
- Logs::with_log_directives merges custom targets with the default zksync=info. Set RUST_LOG to override via environment.
- JSON logs install a panic hook so panic payloads, locations, and backtraces are emitted as structured JSON.

## OpenTelemetry

- OpenTelemetry::new(level, traces_url, logs_url) parses endpoints and builds OTLP exporters (tracing-opentelemetry + opentelemetry_appender_tracing).
- OpenTelemetryLevel controls per-layer filters while still honoring the global subscriber filter.
- ServiceDescriptor populates resource attributes from Kubernetes-friendly env vars (`POD_NAME`, `POD_NAMESPACE`, `CLUSTER_NAME`,
  `DEPLOYMENT_ENVIRONMENT`, `SERVICE_NAME`) with local defaults. Use `with_service_descriptor` to override.

## Prometheus Metrics

- Export metrics collected via the vise registry.
- `PrometheusExporterConfig::pull(port)` starts an HTTP endpoint; .push(gateway_uri, interval) pushes to a gateway.
- `PrometheusExporterConfig::gateway_endpoint(base_url)` builds a `/metrics/job/.../namespace/.../pod/...` URL using `POD_NAMESPACE` / `POD_NAME`.
- The exporter expects a Tokio runtime and should run on a dedicated task; stop it via a `watch::Receiver<bool>`.

## Sentry

- `Sentry::new(dsn)` + `with_environment` configures WARN/ERROR span and event forwarding.
- `ObservabilityGuard::force_flush` and `shutdown` run automatically on drop so events are sent even during abrupt shutdowns.

## Component State Reporting

Track how long asynchronous components spend in each state:

```rust
use zksync_os_observability::component_state_reporter::{ComponentStateReporter, StateLabel};
use zksync_os_observability::GenericComponentState;

#[derive(Clone)]
enum MyState {
    Idle,
    Working,
}

impl StateLabel for MyState {
    fn generic(&self) -> GenericComponentState {
        match self {
            MyState::Idle => GenericComponentState::WaitingRecv,
            MyState::Working => GenericComponentState::Processing,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            MyState::Idle => "idle",
            MyState::Working => "working",
        }
    }
}

let handle = ComponentStateReporter::global().handle_for("my_worker", MyState::Idle);
handle.enter_state(MyState::Working);
```

The background task updates `GENERAL_METRICS.component_time_spent_in_state[component, generic_state, specific_state]` every two seconds.

## Built-in Metrics

metrics::GENERAL_METRICS exposes:

- component_time_spent_in_state (counter in seconds by component / generic state / specific state),
- process_started_at (timestamp gauge with version + role),
- startup_time (startup stage durations),
- fee_collector_address and chain_id.

Access values through GENERAL_METRICS.get() or use them from other crates in the workspace.

## Latency Distribution Tracking

LatencyDistributionTracker records sequential pipeline stages and formats them with percentage breakdowns—ideal for logging batch-processing latency:

```rust
use zksync_os_observability::LatencyDistributionTracker;

let mut tracker = LatencyDistributionTracker::default();
tracker.record_stage("execute_l1", |duration| tracing::info!(?duration, "stage done"));
tracing::info!("latency: {tracker}");
```

The formatter sorts stages by duration and prints aggregate totals for quick post-mortem analysis.
