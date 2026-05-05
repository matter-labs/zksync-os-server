//! Prometheus-related functionality, such as [`PrometheusExporterConfig`].

use std::{env, net::Ipv4Addr, time::Duration};

use anyhow::Context as _;
use reth_tasks::shutdown::GracefulShutdown;
use vise::{MetricsCollection, Registry};
use vise_exporter::MetricsExporter;

use crate::tokio_runtime;

#[derive(Debug, Clone)]
enum PrometheusTransport {
    Pull {
        port: u16,
    },
    Push {
        gateway_uri: String,
        interval: Duration,
    },
}

/// Configuration of a Prometheus exporter.
#[derive(Debug, Clone)]
pub struct PrometheusExporterConfig {
    transport: PrometheusTransport,
}

impl PrometheusExporterConfig {
    /// Creates an exporter that will run an HTTP server on the specified `port`.
    pub const fn pull(port: u16) -> Self {
        Self {
            transport: PrometheusTransport::Pull { port },
        }
    }

    /// Creates an exporter that will push metrics to the specified Prometheus gateway endpoint.
    pub const fn push(gateway_uri: String, interval: Duration) -> Self {
        Self {
            transport: PrometheusTransport::Push {
                gateway_uri,
                interval,
            },
        }
    }

    /// Creates a full push gateway endpoint.
    pub fn gateway_endpoint(base_url: &str) -> String {
        let job_id = "zksync-pushgateway";
        let namespace =
            env::var("POD_NAMESPACE").unwrap_or_else(|_| "UNKNOWN_NAMESPACE".to_owned());
        let pod = env::var("POD_NAME").unwrap_or_else(|_| "UNKNOWN_POD".to_owned());
        format!("{base_url}/metrics/job/{job_id}/namespace/{namespace}/pod/{pod}")
    }

    fn registry(&self) -> Registry {
        let is_push_exporter = matches!(self.transport, PrometheusTransport::Push { .. });
        MetricsCollection::lazy()
            .filter(|group| (group.name == "PushMetrics") == is_push_exporter)
            .collect()
    }

    /// Runs the exporter. This future should be spawned in a separate Tokio task.
    pub async fn run(self, shutdown: GracefulShutdown) -> anyhow::Result<()> {
        tokio_runtime::register_monitor();
        let registry = self.registry();
        let metrics_exporter = MetricsExporter::new(registry.into())
            .with_graceful_shutdown(shutdown.clone().ignore_guard());

        match self.transport {
            PrometheusTransport::Pull { port } => {
                let prom_bind_address = (Ipv4Addr::UNSPECIFIED, port).into();
                metrics_exporter
                    .start(prom_bind_address)
                    .await
                    .context("Failed starting metrics server")?;
            }
            PrometheusTransport::Push {
                gateway_uri,
                interval,
            } => {
                let endpoint = gateway_uri
                    .parse()
                    .context("Failed parsing Prometheus push gateway endpoint")?;
                metrics_exporter.push_to_gateway(endpoint, interval).await;
            }
        }
        drop(shutdown);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str, time::Duration};

    use reth_tasks::{RuntimeBuilder, RuntimeConfig};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        runtime::Handle,
        sync::oneshot,
    };

    use super::*;
    use crate::PUSH_METRICS;

    #[test]
    fn splits_pull_and_push_registries() {
        let pull_registry = PrometheusExporterConfig::pull(0).registry();
        let push_registry =
            PrometheusExporterConfig::push("http://127.0.0.1".to_owned(), Duration::from_secs(1))
                .registry();

        assert!(
            pull_registry
                .descriptors()
                .metric("last_revm_divergence_timestamp_push")
                .is_none()
        );
        assert!(
            push_registry
                .descriptors()
                .metric("last_revm_divergence_timestamp_push")
                .is_some()
        );
        assert!(pull_registry.descriptors().metric("chain_id").is_some());
        assert!(push_registry.descriptors().metric("chain_id").is_none());
    }

    #[tokio::test]
    async fn push_exporter_pushes_before_shutdown_finishes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (request_sender, request_receiver) = oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_http_request_body(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            request_sender.send(body).ok();
        });

        PUSH_METRICS.last_revm_divergence_timestamp_push.set(1);

        let runtime = RuntimeBuilder::new(RuntimeConfig::with_existing_handle(Handle::current()))
            .build()
            .unwrap();
        let (completion_sender, completion_receiver) = oneshot::channel();
        let endpoint = format!("http://{local_addr}/metrics");
        runtime.spawn_critical_with_graceful_shutdown_signal(
            "test prometheus push",
            |shutdown| async move {
                PrometheusExporterConfig::push(endpoint, Duration::from_secs(60))
                    .run(shutdown)
                    .await
                    .unwrap();
                completion_sender.send(()).ok();
            },
        );

        let runtime_for_shutdown = runtime.clone();
        tokio::task::spawn_blocking(move || {
            assert!(runtime_for_shutdown.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        })
        .await
        .unwrap();

        let body = tokio::time::timeout(Duration::from_secs(5), request_receiver)
            .await
            .expect("timed out waiting for push request")
            .unwrap();
        let body = str::from_utf8(&body).unwrap();
        assert!(body.contains("last_revm_divergence_timestamp_push"));

        tokio::time::timeout(Duration::from_secs(5), completion_receiver)
            .await
            .expect("timed out waiting for exporter completion")
            .unwrap();
    }

    async fn read_http_request_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let headers_end = loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(
                read, 0,
                "connection closed before request headers were complete"
            );
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(headers_end) = find_subslice(&buffer, b"\r\n\r\n") {
                break headers_end + 4;
            }
        };

        let headers = str::from_utf8(&buffer[..headers_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .expect("request must include content-length");

        while buffer.len() - headers_end < content_length {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(
                read, 0,
                "connection closed before request body was complete"
            );
            buffer.extend_from_slice(&chunk[..read]);
        }

        buffer[headers_end..headers_end + content_length].to_vec()
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
