use alloy::rpc::types::trace::geth::GethDebugTracerConfig;
use serde::Deserialize;

/// Configuration of the built-in `callTracer`.
///
/// Mirrors geth's `CallConfig` (`onlyTopCall`, `withLog`) and adds ZKsync OS specific options.
/// Unknown fields are ignored, as in geth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallTracerConfig {
    /// Only trace the top-level call(s), skipping sub-calls.
    pub only_top_call: bool,
    /// Include logs emitted by the calls.
    pub with_log: bool,
    /// Return every top-level call of an L1->L2 transaction, not only the main one.
    ///
    /// While processing an L1->L2 transaction the bootloader makes auxiliary calls (to the
    /// `L2AssetTracker`) that show up as additional root frames. By default they are dropped and
    /// only the main transaction frame is returned. When enabled, every root frame is returned,
    /// in execution order, as a child of a synthetic frame that mirrors the transaction.
    /// Traces with a single root frame are returned as-is.
    pub include_all_l1_tx_calls: bool,
}

/// Wire format of [`CallTracerConfig`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCallTracerConfig {
    #[serde(default)]
    only_top_call: Option<bool>,
    #[serde(default)]
    with_log: Option<bool>,
    #[serde(default)]
    include_all_l1_tx_calls: Option<bool>,
}

impl CallTracerConfig {
    /// Parses the `tracerConfig` object of a `callTracer` request. A `null` config means defaults.
    pub fn from_tracer_config(config: GethDebugTracerConfig) -> Result<Self, serde_json::Error> {
        if config.is_null() {
            return Ok(Self::default());
        }
        let raw: RawCallTracerConfig = config.from_value()?;
        Ok(Self {
            only_top_call: raw.only_top_call.unwrap_or_default(),
            with_log: raw.with_log.unwrap_or_default(),
            include_all_l1_tx_calls: raw.include_all_l1_tx_calls.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::types::trace::geth::GethDebugTracerConfig;
    use serde_json::json;

    #[test]
    fn parses_all_fields() {
        let config = CallTracerConfig::from_tracer_config(GethDebugTracerConfig(json!({
            "onlyTopCall": true,
            "withLog": true,
            "includeAllL1TxCalls": true,
        })))
        .unwrap();

        assert_eq!(
            config,
            CallTracerConfig {
                only_top_call: true,
                with_log: true,
                include_all_l1_tx_calls: true,
            }
        );
    }

    #[test]
    fn defaults_missing_fields_to_false() {
        let config = CallTracerConfig::from_tracer_config(GethDebugTracerConfig(json!({
            "onlyTopCall": true,
        })))
        .unwrap();

        assert_eq!(
            config,
            CallTracerConfig {
                only_top_call: true,
                with_log: false,
                include_all_l1_tx_calls: false,
            }
        );
    }

    #[test]
    fn null_config_is_default() {
        let config =
            CallTracerConfig::from_tracer_config(GethDebugTracerConfig(serde_json::Value::Null))
                .unwrap();

        assert_eq!(config, CallTracerConfig::default());
    }

    #[test]
    fn ignores_unknown_fields_like_geth() {
        let config = CallTracerConfig::from_tracer_config(GethDebugTracerConfig(json!({
            "somethingElse": 1,
        })))
        .unwrap();

        assert_eq!(config, CallTracerConfig::default());
    }

    #[test]
    fn rejects_non_boolean_flag() {
        let result = CallTracerConfig::from_tracer_config(GethDebugTracerConfig(json!({
            "includeAllL1TxCalls": "yes",
        })));

        assert!(result.is_err());
    }
}
