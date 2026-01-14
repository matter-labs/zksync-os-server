use alloy::primitives::B256;
use alloy::signers::k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use smart_config::ErrorWithOrigin;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};

/// Custom deserializer for `ecdsa::SigningKey`.
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
