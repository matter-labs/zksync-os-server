//! ZKsync OS Socket Utilities
//!
//! This crate provides common TCP connection utilities for ZKsync OS components.
//! Each connection is established with retry logic and HTTP-like handshake to
//! work with HTTP load balancers. Its then dropped to raw TCP that is handled
//! depending on component implementation

use anyhow::Context as _;
use backon::ExponentialBuilder;
use backon::Retryable;
use std::fmt::Display;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, ToSocketAddrs};

/// Connects to a TCP server with retry logic and performs HTTP handshake.
///
/// This function uses exponential backoff retry logic with hardcoded parameters
/// After establishing the TCP connection, it automatically performs an HTTP-like
/// handshake.
pub async fn connect<A: ToSocketAddrs + Display>(
    address: A,
    path: &str,
) -> anyhow::Result<TcpStream> {
    let mut socket = (|| TcpStream::connect(&address))
        .retry(
            ExponentialBuilder::default()
                .with_factor(2.0)
                .with_min_delay(Duration::from_secs(1))
                .with_max_delay(Duration::from_secs(20))
                .with_max_times(15),
        )
        .notify(|err, dur| {
            tracing::info!(
                ?err,
                ?dur,
                "retrying connection to server {}{}",
                &address,
                path
            );
        })
        .await
        .context("Failed to connect to server")?;

    // Perform HTTP handshake
    let handshake = format!("POST {path} HTTP/1.0\r\n\r\n");
    socket
        .write_all(handshake.as_bytes())
        .await
        .context("Failed to write HTTP handshake")?;

    Ok(socket)
}

use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;

pub async fn skip_http_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<(), std::io::Error> {
    // Detects two consecutive line endings, which may be \r\n or \n.
    let mut empty_line = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF reached before end of headers",
            ));
        }

        for (i, &byte) in buf.iter().enumerate() {
            if byte == b'\n' {
                if empty_line {
                    reader.consume(i + 1);
                    return Ok(());
                }
                empty_line = true;
            } else if byte != b'\r' {
                empty_line = false;
            }
        }

        let len = buf.len();
        reader.consume(len);
    }
}
