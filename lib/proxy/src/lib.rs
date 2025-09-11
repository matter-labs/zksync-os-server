use std::sync::Arc;

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    try_join,
};

#[derive(Default)]
pub struct Routes(Vec<(&'static [u8], &'static [u8], u16)>);

impl Routes {
    pub fn add_route(
        mut self,
        method: &'static [u8],
        path_prefix: &'static [u8],
        port: u16,
    ) -> Self {
        self.0.push((method, path_prefix, port));
        self
    }

    fn find(&self, (method, path): (&[u8], &[u8])) -> Option<u16> {
        self.0.iter().find_map(|(m, prefix, port)| {
            (*m == method && path.starts_with(prefix)).then_some(*port)
        })
    }
}

/// Starts a server on `port` that routes any incoming TCP connections to other ports.
/// The `routes` parameter takes an array of routes consisting of method, path and port to route to.
pub async fn run_proxy(address: impl ToSocketAddrs, routes: Routes) -> Result<()> {
    let routes = Arc::new(routes);

    let listener = TcpListener::bind(address).await?;

    loop {
        let mut connection = listener.accept().await?.0;

        let routes = routes.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 256];
            let mut bytes_read = 0;
            let method_and_path = loop {
                bytes_read += connection.read(&mut buffer[bytes_read..]).await.unwrap();

                if let Some(parts) = get_method_and_path(&buffer[..bytes_read]) {
                    break parts;
                }

                if bytes_read == buffer.len() {
                    tracing::info!(
                        "Proxy could not parse request start: {:?}",
                        &buffer[..bytes_read]
                    );
                    let _ = connection
                        .write_all(b"HTTP/1.1 414 \r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
            };

            let Some(target_port) = routes.find(method_and_path) else {
                tracing::info!(
                    "No route for method {:?} and path {:?} in proxy",
                    method_and_path.0,
                    method_and_path.1
                );
                let _ = connection
                    .write_all(b"HTTP/1.1 404 \r\nConnection: close\r\n\r\n")
                    .await;
                return;
            };

            let mut target = TcpStream::connect(("127.0.0.1", target_port))
                .await
                .expect("proxy failed to connect to target");
            let (mut conn_source, mut conn_sink) = connection.split();
            let (mut target_source, mut target_sink) = target.split();

            try_join!(
                async {
                    target_sink.write_all(&buffer[..bytes_read]).await.unwrap();
                    tokio::io::copy(&mut conn_source, &mut target_sink).await
                },
                tokio::io::copy(&mut target_source, &mut conn_sink)
            )
            .expect("proxy encountered error while forwarding data");
        });
    }
}

fn get_method_and_path(request: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut iter = request.split(|b| *b == b' ').filter(|t| !t.is_empty());
    let method = iter.next()?;
    let path = iter.next()?;
    // Can't know if the path is finished if the file ends there.
    iter.next()?;

    Some((method, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_method_and_path() {
        assert_eq!(
            get_method_and_path(b"GET /path HTTP/1.1"),
            Some((b"GET".as_slice(), b"/path".as_slice()))
        );
        assert_eq!(
            get_method_and_path(b"GET  /path HTTP/1.1"),
            Some((b"GET".as_slice(), b"/path".as_slice()))
        );
        assert_eq!(get_method_and_path(b"GET /pat"), None);
        assert_eq!(
            get_method_and_path(b" POST  /a HTTP/1.1"),
            Some((b"POST".as_slice(), b"/a".as_slice()))
        );
    }
}
