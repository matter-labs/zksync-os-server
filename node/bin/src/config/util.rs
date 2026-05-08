use super::metrics::CONFIG_METRICS;
use alloy::primitives::B256;
use alloy::signers::k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};
use smart_config::{DescribeConfig, ErrorWithOrigin, SerializerOptions};
use zksync_os_network::SecretKey;
use zksync_os_operator_signer::SignerConfig;

pub fn report_flat_config_metrics<C: DescribeConfig>(config: &C, prefix: &str) {
    for (name, value) in flat_config_metric_entries(config, prefix) {
        CONFIG_METRICS.values[&(name, value)].set(1);
    }
}

fn flat_config_metric_entries<C: DescribeConfig>(
    config: &C,
    prefix: &str,
) -> Vec<(String, String)> {
    SerializerOptions::default()
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

/// Custom deserializer for [`SignerConfig`].
///
/// Accepts either:
/// - A hex string (with or without `0x` prefix): parsed as a local private key (backward-compatible)
/// - An object `{"type": "gcp_kms", "resource": "projects/..."}`: a GCP KMS key
#[derive(Debug)]
pub struct SignerConfigDeserializer;

impl DeserializeParam<SignerConfig> for SignerConfigDeserializer {
    const EXPECTING: BasicTypes = BasicTypes::STRING.or(BasicTypes::OBJECT);

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<SignerConfig, ErrorWithOrigin> {
        let deserializer = ctx.current_value_deserializer(param.name)?;
        let value = Value::deserialize(deserializer)?;

        match value {
            Value::String(s) => {
                // Backward-compatible: plain hex string = local private key
                let b256: B256 =
                    serde_json::from_value(Value::String(s)).map_err(ErrorWithOrigin::custom)?;
                let sk =
                    SigningKey::from_slice(b256.as_slice()).map_err(ErrorWithOrigin::custom)?;
                Ok(SignerConfig::Local(sk))
            }
            Value::Object(obj) => {
                let type_str = obj.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
                    ErrorWithOrigin::custom("missing 'type' field in signer config")
                })?;
                match type_str {
                    "gcp_kms" => {
                        let resource =
                            obj.get("resource")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    ErrorWithOrigin::custom(
                                        "missing 'resource' field in gcp_kms signer config",
                                    )
                                })?;
                        Ok(SignerConfig::gcp_kms(resource.to_string()))
                    }
                    other => Err(ErrorWithOrigin::custom(format!(
                        "unknown signer type '{other}', expected 'gcp_kms'"
                    ))),
                }
            }
            _ => Err(ErrorWithOrigin::custom(
                "expected a hex string (local key) or an object with 'type' field",
            )),
        }
    }

    fn serialize_param(&self, param: &SignerConfig) -> Value {
        match param {
            SignerConfig::Local(sk) => {
                let bytes = B256::from_slice(sk.to_bytes().as_slice());
                serde_json::to_value(bytes).expect("failed serializing to JSON")
            }
            SignerConfig::GcpKms { resource_name, .. } => {
                serde_json::json!({"type": "gcp_kms", "resource": resource_name})
            }
        }
    }
}

/// Custom deserializer for `secp256k1::SecretKey`.
///
/// Accepts hex strings both with and without `0x` prefix.
/// The built-in `secp256k1` string parser does not support the `0x` prefix,
/// so we go through `B256` (which uses `const-hex` and strips the prefix automatically).
#[derive(Debug)]
pub struct SecretKeyDeserializer;

impl DeserializeParam<SecretKey> for SecretKeyDeserializer {
    const EXPECTING: BasicTypes = BasicTypes::STRING;

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<SecretKey, ErrorWithOrigin> {
        let deserializer = ctx.current_value_deserializer(param.name)?;
        let b256 = B256::deserialize(deserializer)?;
        SecretKey::from_slice(b256.as_slice()).map_err(ErrorWithOrigin::custom)
    }

    fn serialize_param(&self, param: &SecretKey) -> Value {
        let bytes = B256::from_slice(&param.secret_bytes());
        serde_json::to_value(bytes).expect("failed serializing to JSON")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::GeneralConfig;

    use super::flat_config_metric_entries;

    #[test]
    fn flat_config_metric_entries_prefix_keys_and_stringify_values() {
        let entries: HashMap<_, _> =
            flat_config_metric_entries(&GeneralConfig::default(), "general")
                .into_iter()
                .collect();

        assert_eq!(entries["general##node_role"], "main");
        assert_eq!(entries["general##gateway_chain_id"], "506");
        assert_eq!(entries["general##run_priority_tree"], "true");
        assert_eq!(entries["general##force_starting_block_number"], "null");
    }
}
