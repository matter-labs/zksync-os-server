//! Prometheus-related functionality, such as [`PrometheusExporterConfig`].

use std::net::Ipv4Addr;

use anyhow::Context as _;
use tokio::sync::{oneshot, watch};
use vise::MetricsCollection;
use vise_exporter::MetricsExporter;

/// Configuration of a Prometheus exporter.
#[derive(Debug, Clone, Default)]
pub struct PrometheusExporterConfig;

impl PrometheusExporterConfig {
    /// Runs the exporter. This future should be spawned in a separate Tokio task.
    pub async fn run(
        self,
        port_sink: oneshot::Sender<u16>,
        mut stop_receiver: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let registry = MetricsCollection::lazy().collect();
        let metrics_exporter =
            MetricsExporter::new(registry.into()).with_graceful_shutdown(async move {
                stop_receiver.changed().await.ok();
            });

        let prom_bind_address = (Ipv4Addr::UNSPECIFIED, 0).into();
        let metrics_server = metrics_exporter
            .bind(prom_bind_address)
            .await
            .context("Failed starting metrics server")?;
        port_sink.send(metrics_server.local_addr().port()).unwrap();
        metrics_server.start().await?;

        Ok(())
    }
}
