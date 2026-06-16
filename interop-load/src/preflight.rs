use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use serde::Serialize;

use crate::config::Config;
use crate::json_rpc;

#[derive(Debug, Serialize)]
pub struct PreflightReport {
    pub chain_a_id: u64,
    pub chain_b_id: u64,
    pub source_chain_ids: Vec<u64>,
    pub destination_chain_ids: Vec<u64>,
    pub gateway_chain_id: u64,
    pub l1_chain_id: u64,
    pub smoke_test_skipped: bool,
    pub smoke_test_skip_reason: Option<String>,
    pub metrics_enabled: bool,
    pub metrics: Vec<MetricEndpointReport>,
    pub required_wallets: usize,
    pub configured_wallets: usize,
}

#[derive(Debug, Serialize)]
pub struct MetricEndpointReport {
    pub url: String,
    pub reachable: bool,
}

pub async fn run(config: &Config) -> anyhow::Result<PreflightReport> {
    validate_private_key_shape(config.rich_privkey.expose())?;

    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .context("failed to build HTTP client")?;

    let (chain_a_id, chain_b_id, gateway_chain_id, l1_chain_id) = tokio::try_join!(
        json_rpc::chain_id(&client, &config.chain_a_rpc),
        json_rpc::chain_id(&client, &config.chain_b_rpc),
        json_rpc::chain_id(&client, &config.gateway_rpc),
        json_rpc::chain_id(&client, &config.l1_rpc),
    )?;
    let mut source_chain_ids = Vec::new();
    for source_rpc in config.source_rpcs() {
        source_chain_ids.push(json_rpc::chain_id(&client, &source_rpc).await?);
    }
    let destination_chain_ids = if config.ring {
        let mut ids = source_chain_ids.clone();
        ids.rotate_left(1);
        ids
    } else {
        vec![chain_b_id; source_chain_ids.len()]
    };
    validate_source_topology(
        config.ring,
        chain_b_id,
        &source_chain_ids,
        &destination_chain_ids,
    )?;

    let required_wallets = config.required_wallets();
    if config.strict_wallet_sizing && config.wallets < required_wallets {
        anyhow::bail!(
            "wallet sizing preflight failed: configured {}, required {} for rate {}",
            config.wallets,
            required_wallets,
            config.rate
        );
    }

    let mut metrics = Vec::with_capacity(config.metrics_url.len());
    for url in &config.metrics_url {
        let reachable = client
            .get(url)
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false);
        if !reachable {
            anyhow::bail!("metrics endpoint is not reachable: {url}");
        }
        metrics.push(MetricEndpointReport {
            url: url.clone(),
            reachable,
        });
    }

    let smoke_test_skip_reason = if config.skip_smoke_test {
        Some("--skip-smoke-test".to_string())
    } else {
        Some("scaffold: RPC smoke flow not implemented yet".to_string())
    };

    Ok(PreflightReport {
        chain_a_id,
        chain_b_id,
        source_chain_ids,
        destination_chain_ids,
        gateway_chain_id,
        l1_chain_id,
        smoke_test_skipped: true,
        smoke_test_skip_reason,
        metrics_enabled: !config.metrics_url.is_empty(),
        metrics,
        required_wallets,
        configured_wallets: config.wallets,
    })
}

fn validate_private_key_shape(value: &str) -> anyhow::Result<()> {
    let hex = value.strip_prefix("0x").unwrap_or(value);
    anyhow::ensure!(
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "--rich-privkey must be a 32-byte hex private key"
    );
    Ok(())
}

fn validate_source_topology(
    ring: bool,
    chain_b_id: u64,
    source_chain_ids: &[u64],
    destination_chain_ids: &[u64],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        source_chain_ids.len() == destination_chain_ids.len(),
        "source and destination lane counts differ"
    );
    let mut seen = HashSet::with_capacity(source_chain_ids.len());
    for (idx, (source_chain_id, destination_chain_id)) in source_chain_ids
        .iter()
        .copied()
        .zip(destination_chain_ids.iter().copied())
        .enumerate()
    {
        anyhow::ensure!(
            ring || source_chain_id != chain_b_id,
            "--source-rpc entry {idx} resolves to destination chain {chain_b_id}; \
             remove --chain-b-rpc from the source list"
        );
        anyhow::ensure!(
            source_chain_id != destination_chain_id,
            "lane {idx} resolves to a self-lane on chain {source_chain_id}"
        );
        if !ring {
            anyhow::ensure!(
                seen.insert(source_chain_id),
                "--source-rpc contains duplicate chain id {source_chain_id}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_topology_rejects_destination_chain() {
        let err = validate_source_topology(false, 6566, &[6565, 6566, 6567], &[6566, 6566, 6566])
            .unwrap_err()
            .to_string();
        assert!(err.contains("resolves to destination chain 6566"));
    }

    #[test]
    fn source_topology_rejects_duplicate_sources() {
        let err = validate_source_topology(false, 6566, &[6565, 6567, 6565], &[6566, 6566, 6566])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate chain id 6565"));
    }

    #[test]
    fn source_topology_allows_destination_chain_in_ring() {
        validate_source_topology(true, 6566, &[6565, 6566, 6567], &[6566, 6567, 6565]).unwrap();
    }

    #[test]
    fn source_topology_allows_duplicate_sources_in_ring() {
        validate_source_topology(
            true,
            6566,
            &[6565, 6568, 6566, 6569, 6567, 6568, 6569],
            &[6568, 6566, 6569, 6567, 6568, 6569, 6565],
        )
        .unwrap();
    }

    #[test]
    fn source_topology_rejects_self_lane() {
        let err = validate_source_topology(true, 6566, &[6565, 6566], &[6565, 6566])
            .unwrap_err()
            .to_string();
        assert!(err.contains("self-lane"));
    }
}
