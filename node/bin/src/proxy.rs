use anyhow::Result;
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    try_join,
};

/// Starts a server on `port` that routes any incoming TCP connections to other ports.
/// The `routes` parameter takes an array of routes consisting of method, path and port to route to.
pub async fn run_proxy<const N: usize>(
    port: u16,
    routes: [(&'static [u8], &'static [u8], u16); N],
) -> Result<()> {
    let route_map = routes
        .iter()
        .map(|(method, path, port)| ((*method, *path), *port))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(route_map.len(), routes.len(), "duplicate routes");
    let routes = Arc::new(route_map);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;

    loop {
        let mut connection = listener.accept().await?.0;

        let routes = routes.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 256];
            let mut bytes_read = 0;
            let method_and_path = loop {
                bytes_read += connection.read(&mut buffer[bytes_read..]).await.unwrap();

                // This isn't quite general enough, technically the same path can be written in multiple ways
                let parts = buffer[..bytes_read]
                    .split(|b| *b == b' ')
                    .take(2)
                    .collect::<Vec<_>>();
                if parts.len() == 2 {
                    break (parts[0], parts[1]);
                }

                if bytes_read == buffer.len() {
                    // We read 256 bytes and still don't have a full request line
                    return;
                }
            };

            let Some(&target_port) = routes.get(&method_and_path) else {
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
