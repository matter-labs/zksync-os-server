use alloy::primitives::B256;
use alloy::signers::k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use smart_config::ErrorWithOrigin;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};
use zksync_os_network::SecretKey;

/// Custom deserializer for `ecdsa::SigningKey`.
///
/// Accepts hex strings both with and without `0x` prefix.
#[derive(Debug)]
pub struct SigningKeyDeserializer;

impl DeserializeParam<SigningKey> for SigningKeyDeserializer {
    const EXPECTING: BasicTypes = BasicTypes::STRING;

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<SigningKey, ErrorWithOrigin> {
        let deserializer = ctx.current_value_deserializer(param.name)?;

        let b256 = B256::deserialize(deserializer)?;
        SigningKey::from_slice(b256.as_slice()).map_err(ErrorWithOrigin::custom)
    }

    fn serialize_param(&self, param: &SigningKey) -> Value {
        let bytes = B256::from_slice(param.to_bytes().as_slice());
        serde_json::to_value(bytes).expect("failed serializing to JSON")
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
