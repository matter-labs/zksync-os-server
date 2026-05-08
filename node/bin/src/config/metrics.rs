use serde_json::Value;
use smart_config::{DescribeConfig, SerializerOptions};
use vise::{Gauge, LabeledFamily, Metrics};

use super::Config;

#[derive(Debug, Metrics)]
#[metrics(prefix = "config")]
pub(super) struct ConfigMetrics {
    #[metrics(labels = ["name", "value"])]
    pub values: LabeledFamily<(String, String), Gauge, 2>,
}

#[vise::register]
pub(super) static CONFIG_METRICS: vise::Global<ConfigMetrics> = vise::Global::new();

pub(crate) fn report_static_config_metrics(config: &Config) {
    report_flat_config_metrics(&config.general_config, "general");
    report_flat_config_metrics(&config.l1_provider_config, "l1_provider");
    report_opt_flat_config_metrics(config.gateway_provider_config.as_ref(), "gateway_provider");
    report_flat_config_metrics(&config.network_config, "network");
    report_flat_config_metrics(&config.genesis_config, "genesis");
    report_flat_config_metrics(&config.rpc_config, "rpc");
    report_flat_config_metrics(&config.mempool_config, "mempool");
    report_flat_config_metrics(&config.tx_validator_config, "tx_validator");
    report_flat_config_metrics(&config.sequencer_config, "sequencer");
    report_flat_config_metrics(&config.l1_sender_config, "l1_sender");
    report_flat_config_metrics(&config.l1_watcher_config, "l1_watcher");
    report_flat_config_metrics(&config.batcher_config, "batcher");
    report_flat_config_metrics(
        &config.prover_input_generator_config,
        "prover_input_generator",
    );
    report_flat_config_metrics(&config.prover_api_config, "prover_api");
    report_flat_config_metrics(&config.status_server_config, "status_server");
    report_flat_config_metrics(&config.observability_config, "observability");
    report_flat_config_metrics(&config.gas_adjuster_config, "gas_adjuster");
    report_flat_config_metrics(&config.batch_verification_config, "batch_verification");
    report_flat_config_metrics(
        &config.base_token_price_updater_config,
        "base_token_price_updater",
    );
    report_flat_config_metrics(&config.interop_fee_updater_config, "interop_fee_updater");
    report_opt_flat_config_metrics(
        config.external_price_api_client_config.as_ref(),
        "external_price_api_client",
    );
    report_flat_config_metrics(&config.fee_config, "fee");
    report_flat_config_metrics(&config.backpressure_config, "backpressure");
}

fn report_flat_config_metrics<C: DescribeConfig>(config: &C, prefix: &str) {
    for (name, value) in flat_config_metric_entries(config, prefix) {
        CONFIG_METRICS.values[&(name, value)].set(1);
    }
}

fn report_opt_flat_config_metrics<C: DescribeConfig>(config: Option<&C>, prefix: &str) {
    if let Some(config) = config {
        report_flat_config_metrics(config, prefix);
    }
}

fn flat_config_metric_entries<C: DescribeConfig>(
    config: &C,
    prefix: &str,
) -> Vec<(String, String)> {
    SerializerOptions::default()
        .with_secret_placeholder("<secret>")
        .flat(true)
        .serialize(config)
        .into_iter()
        .map(|(key, value)| {
            let name = format!("{prefix}_{key}");
            let value = stringify_config_value(value);
            (name, value)
        })
        .collect()
}

fn stringify_config_value(value: Value) -> String {
    match value {
        Value::String(value) => value,
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use alloy::signers::k256::ecdsa::SigningKey;
    use smart_config::metadata::EtherUnit;
    use zksync_os_operator_signer::SignerConfig;
    use zksync_os_types::PubdataMode;

    use crate::config::{ForceTransactionResubmissionConfig, GeneralConfig, L1SenderConfig};

    use super::flat_config_metric_entries;

    #[test]
    fn flat_config_metric_entries_prefix_keys_and_stringify_values() {
        let entries: HashMap<_, _> =
            flat_config_metric_entries(&GeneralConfig::default(), "general")
                .into_iter()
                .collect();

        assert_eq!(entries["general_node_role"], "main");
        assert_eq!(entries["general_gateway_chain_id"], "506");
        assert_eq!(entries["general_run_priority_tree"], "true");
        assert_eq!(entries["general_force_starting_block_number"], "null");
    }

    #[test]
    fn flat_config_metric_entries_redact_secret_values() {
        let config = L1SenderConfig {
            operator_commit_sk: Some(local_signer()),
            operator_prove_sk: None,
            operator_execute_sk: None,
            max_fee_per_gas: 200 * EtherUnit::Gwei,
            max_priority_fee_per_gas: 1 * EtherUnit::Gwei,
            max_fee_per_blob_gas: 2 * EtherUnit::Gwei,
            force_transaction_resubmission: ForceTransactionResubmissionConfig::default(),
            command_limit: 16,
            poll_interval: Duration::from_secs(1),
            transaction_timeout: Duration::from_secs(600),
            fusaka_upgrade_timestamp: u64::MAX,
            enabled: true,
            pubdata_mode: Some(PubdataMode::Blobs),
            max_batch_diff_to_upstream: None,
        };
        let entries: HashMap<_, _> = flat_config_metric_entries(&config, "l1_sender")
            .into_iter()
            .collect();

        assert_eq!(entries["l1_sender_operator_commit_sk"], "<secret>");
    }

    fn local_signer() -> SignerConfig {
        SignerConfig::Local(SigningKey::from_slice(&[0x11; 32]).unwrap())
    }
}
