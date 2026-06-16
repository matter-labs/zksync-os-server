use anyhow::{Context, bail};
use reqwest::Client;
use serde_json::{Value, json};

pub async fn block_number(client: &Client, url: &str) -> anyhow::Result<u64> {
    let value = rpc(client, url, "eth_blockNumber", json!([])).await?;
    let hex = value
        .as_str()
        .context("eth_blockNumber response result is not a string")?;
    parse_hex_u64(hex)
}

pub async fn chain_id(client: &Client, url: &str) -> anyhow::Result<u64> {
    let value = rpc(client, url, "eth_chainId", json!([])).await?;
    let hex = value
        .as_str()
        .context("eth_chainId response result is not a string")?;
    parse_hex_u64(hex)
}

pub async fn rpc(client: &Client, url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let response = client
        .post(url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .with_context(|| format!("failed to call {method} on {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        bail!("{method} on {url} returned HTTP {status}: {body}");
    }
    let body = response
        .json::<Value>()
        .await
        .with_context(|| format!("failed to decode {method} response from {url}"))?;
    if let Some(error) = body.get("error") {
        anyhow::bail!("{method} on {url} returned RPC error: {error}");
    }
    body.get("result")
        .cloned()
        .with_context(|| format!("{method} response from {url} has no result"))
}

fn parse_hex_u64(hex: &str) -> anyhow::Result<u64> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    Ok(u64::from_str_radix(hex, 16)?)
}

#[cfg(test)]
mod tests {
    use super::parse_hex_u64;

    #[test]
    fn parses_prefixed_hex_u64() {
        assert_eq!(parse_hex_u64("0x0").unwrap(), 0);
        assert_eq!(parse_hex_u64("0x13").unwrap(), 19);
    }

    #[test]
    fn parses_unprefixed_hex_u64() {
        assert_eq!(parse_hex_u64("2a").unwrap(), 42);
    }

    #[test]
    fn rejects_invalid_hex() {
        assert!(parse_hex_u64("0xzz").is_err());
    }
}
