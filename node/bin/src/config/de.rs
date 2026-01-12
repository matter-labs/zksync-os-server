use serde::Deserialize;
use serde_json::Value;
use smart_config::ErrorWithOrigin;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};
use zksync_os_network::NodeRecord;

#[derive(Debug)]
pub struct NodeRecordVec;

impl DeserializeParam<Vec<NodeRecord>> for NodeRecordVec {
    const EXPECTING: BasicTypes = BasicTypes::STRING;

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<Vec<NodeRecord>, ErrorWithOrigin> {
        let de = ctx.current_value_deserializer(param.name)?;
        let records = Vec::<NodeRecord>::deserialize(de)?;
        Ok(records)
    }

    fn serialize_param(&self, param: &Vec<NodeRecord>) -> Value {
        serde_json::to_value(param).unwrap()
    }
}

#[derive(Debug)]
pub struct SecretKey;

impl DeserializeParam<zksync_os_network::SecretKey> for SecretKey {
    const EXPECTING: BasicTypes = BasicTypes::STRING;

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<zksync_os_network::SecretKey, ErrorWithOrigin> {
        let de = ctx.current_value_deserializer(param.name)?;
        let key = zksync_os_network::SecretKey::deserialize(de)?;
        Ok(key)
    }

    fn serialize_param(&self, param: &zksync_os_network::SecretKey) -> Value {
        serde_json::to_value(param).unwrap()
    }
}
