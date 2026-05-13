use serde_json::Value;
use smart_config::{DescribeConfig, SerializerOptions};
use vise::{EncodeLabelSet, Info, LabeledFamily, Metrics};

use super::Config;

#[derive(Debug, EncodeLabelSet)]
struct ValueLabel {
    value: String,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "config")]
pub(super) struct ConfigMetrics {
    #[metrics(labels = ["name"])]
    values: LabeledFamily<String, Info<ValueLabel>>,
}

#[vise::register]
pub(super) static CONFIG_METRICS: vise::Global<ConfigMetrics> = vise::Global::new();

/// Report every top-level config for metrics
/// Called once during initialization
///
/// If you rename or add a top-level config, you need to change the prefix or register here
pub(crate) fn report_static_config_metrics(config: &Config) {
    report_flat_config_metrics(&config.general_config, "general");
    report_flat_config_metrics(&config.l1_provider_config, "l1_provider");
    report_flat_config_metrics_opt(config.gateway_provider_config.as_ref(), "gateway_provider");
    report_flat_config_metrics(&config.network_config, "network");
    report_flat_config_metrics(&config.genesis_config, "genesis");
    report_flat_config_metrics(&config.rpc_config, "rpc");
    report_flat_config_metrics(&config.mempool_config, "mempool");
    report_flat_config_metrics(&config.tx_validator_config, "tx_validator");
    report_flat_config_metrics(&config.sequencer_config, "sequencer");
    report_flat_config_metrics(&config.l1_sender_config, "l1_sender");
    report_flat_config_metrics(&config.gateway_sender_config, "gateway_sender");
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
    report_flat_config_metrics_opt(
        config.external_price_api_client_config.as_ref(),
        "external_price_api_client",
    );
    report_flat_config_metrics(&config.fee_config, "fee");
    report_flat_config_metrics(&config.backpressure_config, "backpressure");
}

fn report_flat_config_metrics<C: DescribeConfig>(config: &C, prefix: &str) {
    for (key, value) in SerializerOptions::default()
        .with_secret_placeholder("<secret>")
        .flat(true)
        .serialize(config)
    {
        let name = format!("{prefix}_{key}");
        let value = stringify_config_value(value);
        let _ = CONFIG_METRICS.values[&name].set(ValueLabel { value });
    }
}

fn report_flat_config_metrics_opt<C: DescribeConfig>(config: Option<&C>, prefix: &str) {
    if let Some(config) = config {
        report_flat_config_metrics(config, prefix);
    }
}

/// Make the serialized value suitable for prometheus export
/// Some characters can break metrics
fn stringify_config_value(value: Value) -> String {
    let value = match value {
        Value::String(value) => value,
        value => value.to_string(),
    };
    sanitize_label_value(value)
}

fn sanitize_label_value(value: String) -> String {
    value
        .replace('"', "'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    #[test]
    fn stringify_config_value_formats_label_values() {
        assert_eq!(super::stringify_config_value(json!("main")), "main");
        assert_eq!(super::stringify_config_value(json!(506)), "506");
        assert_eq!(super::stringify_config_value(json!(true)), "true");
        assert_eq!(super::stringify_config_value(Value::Null), "null");
        assert_eq!(
            super::stringify_config_value(json!(["0x36615Cf349d7F6344891B1e7CA7C72883F5dc049"])),
            "['0x36615Cf349d7F6344891B1e7CA7C72883F5dc049']"
        );
    }
}
