use crate::sandbox::ERGS_PER_GAS;
use alloy::primitives::{Address, B256, U256};
use boa_engine::{JsError, JsString, JsValue};
use jsonrpsee::core::JsonValue;
use zksync_os_interface::tracing::EvmResources;

pub(crate) fn gas_used_from_resources(resources: EvmResources) -> U256 {
    U256::from(resources.ergs / ERGS_PER_GAS)
}

pub(crate) fn extract_js_source_and_config(js_cfg: String) -> anyhow::Result<(String, JsonValue)> {
    let (source, config) = if let Ok(cfg) = serde_json::from_str::<JsonValue>(&js_cfg) {
        match cfg {
            JsonValue::Object(map) => (
                map.get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                map.get("config")
                    .map(|v| v.to_owned())
                    .unwrap_or(JsonValue::Null),
            ),
            _ => (String::new(), JsonValue::Null),
        }
    } else {
        // this means the config only contains raw JS code as a string
        (js_cfg, JsonValue::Null)
    };

    if source.is_empty() {
        return Err(anyhow::anyhow!(
            "JS tracer source not provided in 'tracer' field"
        ));
    }

    Ok((source, config))
}

pub(crate) fn parse_address(s: &str) -> Option<Address> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::primitives::hex::decode(s).ok()?;
    if bytes.len() != 20 {
        return None;
    }

    Some(Address::from_slice(&bytes))
}

pub(crate) fn parse_b256(s: &str) -> Option<B256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::primitives::hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }

    Some(B256::from_slice(&bytes))
}

pub(crate) fn format_hex_u256(v: U256) -> String {
    if v == U256::ZERO {
        return "0x0".to_string();
    }

    format!("0x{v:x}")
}

pub(crate) fn anyhow_error_to_js_error(e: anyhow::Error) -> JsError {
    JsError::from_opaque(JsValue::from(JsString::from(e.to_string())))
}

pub(crate) fn wrap_js_invocation(body: impl AsRef<str>) -> String {
    let content = body.as_ref().trim_matches('\n');
    format!("(function(){{\n{content}\n}})()")
}
