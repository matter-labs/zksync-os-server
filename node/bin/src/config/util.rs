use alloy::primitives::B256;
use alloy::signers::k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use smart_config::ErrorWithOrigin;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};
use zksync_os_network::SecretKey;
use zksync_os_operator_signer::OperatorSignerConfig;

/// Custom deserializer for [`OperatorSignerConfig`].
///
/// Accepts either:
/// - A hex string (with or without `0x` prefix): parsed as a local private key (backward-compatible)
/// - An object `{"type": "gcp_kms", "resource": "projects/..."}`: a GCP KMS key
#[derive(Debug)]
pub struct OperatorSignerConfigDeserializer;

impl DeserializeParam<OperatorSignerConfig> for OperatorSignerConfigDeserializer {
    const EXPECTING: BasicTypes = BasicTypes::STRING.or(BasicTypes::OBJECT);

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<OperatorSignerConfig, ErrorWithOrigin> {
        let deserializer = ctx.current_value_deserializer(param.name)?;
        let value = Value::deserialize(deserializer)?;

        match value {
            Value::String(s) => {
                // Backward-compatible: plain hex string = local private key
                let b256: B256 =
                    serde_json::from_value(Value::String(s)).map_err(ErrorWithOrigin::custom)?;
                let sk =
                    SigningKey::from_slice(b256.as_slice()).map_err(ErrorWithOrigin::custom)?;
                Ok(OperatorSignerConfig::Local(sk))
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
                        Ok(OperatorSignerConfig::gcp_kms(resource.to_string()))
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

    fn serialize_param(&self, param: &OperatorSignerConfig) -> Value {
        match param {
            OperatorSignerConfig::Local(_) => Value::String("[REDACTED]".to_string()),
            OperatorSignerConfig::GcpKms { resource_name, .. } => {
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
