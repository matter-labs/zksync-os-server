//! A TCP proxy in front of anvil that a test can sever and restore, emulating an L1
//! RPC provider outage. While severed, new connections are refused and in-flight
//! ones are reset — the same failure shape as a real provider outage or a network
//! partition (and the chaos rig's `Disconnect` fault).

use anyhow::Context as _;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

pub struct SeverableL1Proxy {
    listen_addr: SocketAddr,
    upstream: SocketAddr,
    accept_task: Option<JoinHandle<()>>,
}

impl SeverableL1Proxy {
    /// Starts forwarding to `upstream_url` (e.g. anvil's `http://localhost:<port>`)
    /// on a freshly allocated local port.
    pub async fn start(upstream_url: &str) -> anyhow::Result<Self> {
        let host_port = upstream_url
            .trim_start_matches("http://")
            .trim_start_matches("ws://")
            .trim_end_matches('/');
        let resolved: Vec<SocketAddr> = tokio::net::lookup_host(host_port)
            .await
            .with_context(|| format!("cannot resolve upstream address from {upstream_url}"))?
            .collect();
        // `localhost` resolves to `::1` first on some hosts while anvil binds IPv4.
        let upstream: SocketAddr = resolved
            .iter()
            .find(|addr| addr.is_ipv4())
            .or_else(|| resolved.first())
            .copied()
            .with_context(|| format!("no addresses resolved for {upstream_url}"))?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let listen_addr = listener.local_addr()?;
        let accept_task = tokio::spawn(accept_loop(listener, upstream));
        Ok(Self {
            listen_addr,
            upstream,
            accept_task: Some(accept_task),
        })
    }

    /// The URL node configs should use as their L1 RPC endpoint.
    pub fn url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }

    /// Severs L1 connectivity: the listener closes (new connections are refused)
    /// and every in-flight connection is reset.
    pub async fn sever(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
            // Await the abort so the port is actually released and connections are
            // torn down before the caller starts measuring the outage.
            let _ = task.await;
        }
    }

    /// Restores L1 connectivity on the same address the nodes were configured with.
    pub async fn restore(&mut self) -> anyhow::Result<()> {
        if self.accept_task.is_some() {
            return Ok(());
        }
        // The just-aborted task releases the port asynchronously; rebinding can
        // transiently fail.
        let mut attempts = 0;
        let listener = loop {
            match TcpListener::bind(self.listen_addr).await {
                Ok(listener) => break listener,
                Err(err) if attempts < 50 => {
                    attempts += 1;
                    tracing::debug!(%err, "proxy port not yet rebindable; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err.into()),
            }
        };
        self.accept_task = Some(tokio::spawn(accept_loop(listener, self.upstream)));
        Ok(())
    }
}

impl Drop for SeverableL1Proxy {
    fn drop(&mut self) {
        if let Some(task) = &self.accept_task {
            task.abort();
        }
    }
}

async fn accept_loop(listener: TcpListener, upstream: SocketAddr) {
    // Forward tasks live in a JoinSet owned by this task: aborting the accept task
    // drops the set, which aborts (resets) every in-flight connection with it.
    let mut connections = JoinSet::new();
    loop {
        match listener.accept().await {
            Ok((mut inbound, _)) => {
                connections.spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(upstream).await {
                        let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
            Err(err) => {
                tracing::debug!(%err, "proxy accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        // Reap finished forwards without blocking the accept path.
        while connections.try_join_next().is_some() {}
    }
}
