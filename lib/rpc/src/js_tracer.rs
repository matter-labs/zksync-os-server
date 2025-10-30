use alloy::primitives::{Address, B256, Bytes, U256};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::{
    Context as BoaContext, JsArgs, JsError, JsString, JsValue, NativeFunction, Source, js_string,
};
use boa_gc::{Finalize, Trace};
use ruint::aliases::B160;
use serde_json::{Map, Value as JsonValue};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_api::helpers::get_code;
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmTracer,
};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

use crate::sandbox::{ERGS_PER_GAS, fmt_error_msg, maybe_revert_reason};

#[derive(Trace, Finalize)]
struct HostEnvironment<V: ViewState + 'static> {
    #[unsafe_ignore_trace]
    state_view: RefCell<V>,
    #[unsafe_ignore_trace]
    storage_overlay: Rc<RefCell<HashMap<(Address, B256), B256>>>,
    #[unsafe_ignore_trace]
    code_overlay: Rc<RefCell<HashMap<Address, Vec<u8>>>>,
}

/// JS tracer implementation
/// Holds a Boa JS runtime and calls user-provided JS tracer methods when the hooks of zksync-os
/// EVM tracer interface are invoked.
/// Since zksync-os interfaces don't provide state access - we use the state before the execution of
/// each transaction, and maintain overlays for storage and code modifications done during the tx.
///
/// The JS tracer can use the `db` object to query the state via the following interface:
///   - getBalance(address): returns balance of an address
///   - getNonce(address): returns the nonce as hex string
///   - getCode(address): returns code at address
///   - getState(address, slot): returns storage value at slot
pub struct JsTracer {
    // JS runtime
    ctx: BoaContext,
    // User-provided tracer config
    tracer_config: JsonValue,

    // Overlays for storage and code modifications
    pub storage_overlay: Rc<RefCell<HashMap<(Address, B256), B256>>>,
    pub code_overlay: Rc<RefCell<HashMap<Address, Vec<u8>>>>,

    // Depth tracking and per-tx result
    current_depth: u64,
    pub results: Vec<JsonValue>,
}

impl JsTracer {
    pub fn new(state_view: impl ViewState + 'static, js_cfg: JsonValue) -> anyhow::Result<Self> {
        let (tracer_source, tracer_config) = extract_js_source_and_config(js_cfg)?;

        let mut ctx = BoaContext::default();
        let bootstrap = format!(
            "var tracer=(function(){{\n
             tracer={tracer_source};\n
             if (typeof tracer === 'object' && tracer) return tracer;\n
             if (typeof exports === 'object' && exports) {{\n
                 var candidate = exports.tracer || exports.default || exports;\n
                 if (typeof candidate === 'object' && candidate) return candidate;\n
             }}\n
             return undefined;}})();",
        );

        ctx.eval(Source::from_bytes(bootstrap.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer bootstrap error: {e:?}")))?;

        // Prepare shared state for DB methods
        let storage_overlay = Rc::new(RefCell::new(HashMap::<(Address, B256), B256>::new()));
        let code_overlay = Rc::new(RefCell::new(HashMap::new()));

        let host_env = HostEnvironment {
            state_view: RefCell::new(state_view.clone()),
            storage_overlay: Rc::clone(&storage_overlay),
            code_overlay: Rc::clone(&code_overlay),
        };
        let host = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                |_this, args, env, ctx| {
                    let method = args
                        .get_or_undefined(0)
                        .to_string(ctx)?
                        .to_std_string_escaped();

                    let payload = args
                        .get_or_undefined(1)
                        .to_string(ctx)?
                        .to_std_string_escaped();
                    let req: JsonValue = serde_json::from_str(&payload).unwrap_or(JsonValue::Null);
                    let resp = match method.as_str() {
                        "getBalance" => {
                            let addr = req.get("address").and_then(|v| v.as_str()).unwrap_or("");

                            match parse_address(addr) {
                                Some(address) => {
                                    let balance = env
                                        .state_view
                                        .borrow_mut()
                                        .get_account(address)
                                        .ok_or(anyhow::anyhow!(
                                            "Account {address:?} not found in a state"
                                        ))
                                        .map_err(anyhow_error_to_js_error)?
                                        .balance;

                                    format_hex_u256(balance)
                                }
                                None => String::from("0x0"),
                            }
                        }
                        "getNonce" => {
                            let addr = req.get("address").and_then(|v| v.as_str()).unwrap_or("");
                            match parse_address(addr) {
                                Some(address) => {
                                    let nonce = env
                                        .state_view
                                        .borrow_mut()
                                        .account_nonce(address)
                                        .ok_or(anyhow::anyhow!(
                                            "Account {address:?} not found in a state"
                                        ))
                                        .map_err(anyhow_error_to_js_error)?;

                                    format!("0x{nonce:x}")
                                }
                                None => String::from("0x0"),
                            }
                        }
                        "getCode" => {
                            let addr = req.get("address").and_then(|v| v.as_str()).unwrap_or("");

                            match parse_address(addr) {
                                Some(address) => {
                                    if let Some(code) = env.code_overlay.borrow().get(&address) {
                                        format!("0x{}", alloy::primitives::hex::encode(code))
                                    } else {
                                        let code = {
                                            let mut state_view = env.state_view.borrow_mut();
                                            let props = state_view
                                                .get_account(address)
                                                .ok_or(anyhow::anyhow!(
                                                    "Account {address:?} not found in a state"
                                                ))
                                                .map_err(anyhow_error_to_js_error)?;
                                            get_code(&mut *state_view, &props)
                                        };

                                        format!("0x{}", alloy::primitives::hex::encode(code))
                                    }
                                }
                                None => String::from("0x"),
                            }
                        }
                        "getState" => {
                            let addr = req
                                .get("address")
                                .and_then(|v| v.as_str())
                                .expect("Address is not supplied in getState");
                            let slot = req
                                .get("slot")
                                .and_then(|v| v.as_str())
                                .expect("Slot is not supplied in getState");
                            match (parse_address(addr), parse_b256(slot)) {
                                (Some(address), Some(key)) => {
                                    if let Some(value) =
                                        env.storage_overlay.borrow().get(&(address, key))
                                    {
                                        format!("0x{}", alloy::primitives::hex::encode(value.0))
                                    } else {
                                        let flat = derive_flat_storage_key(
                                            &B160::from_be_bytes(address.into_array()),
                                            &(key.0.into()),
                                        );
                                        let value = env
                                            .state_view
                                            .borrow_mut()
                                            .read(B256::from(flat.as_u8_array()))
                                            .unwrap_or_default();

                                        format!("0x{}", alloy::primitives::hex::encode(value.0))
                                    }
                                }
                                _ => String::from("0x0"),
                            }
                        }
                        _ => String::from("null"),
                    };
                    Ok(JsValue::from(js_string!(resp)))
                },
                host_env,
            ),
        )
        .name(js_string!("__hostCall"))
        .length(2)
        .build();

        // Install __hostCall function into global object
        let global_ctx = ctx.global_object();
        global_ctx
            .set(js_string!("__hostCall"), host, false, &mut ctx)
            .expect("set host");

        // Install JS wrapper `db` object that calls __hostCall
        let js_db_wrapper = r#"
            var db = {
                getBalance: function(a){ return __hostCall("getBalance", JSON.stringify({address: a})); },
                getNonce: function(a){ return __hostCall("getNonce", JSON.stringify({address: a})); },
                getCode: function(a){ return __hostCall("getCode", JSON.stringify({address: a})); },
                getState: function(a,s){ return __hostCall("getState", JSON.stringify({address: a, slot: s})); }
            };
        "#;
        ctx.eval(Source::from_bytes(js_db_wrapper.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("install db wrapper failed: {e:?}")))?;

        Ok(Self {
            ctx,
            tracer_config,
            storage_overlay,
            code_overlay,
            current_depth: 0,
            results: Vec::new(),
        })
    }

    /// `call_method` invokes a method on the JS tracer object with the given argument.
    fn call_method(&mut self, method: &str, arg: &JsonValue) -> anyhow::Result<()> {
        let arg_json = serde_json::to_string(arg).unwrap_or("null".to_string());

        // To support the optionality of tracer methods, we check if the method exists before calling it.
        let snippet = format!(
            "(function(){{ if (typeof tracer === 'object' && typeof tracer.{method} === 'function') tracer.{method}({arg_json}) }})()"
        );

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method {method} failed: {e:?}")))?;

        Ok(())
    }

    /// `call_result` is called at the end of the transaction to get the final result from the tracer.
    fn call_result(&mut self) -> anyhow::Result<JsonValue> {
        let snippet = "(function(){ return JSON.stringify(tracer.result()); })()";
        let v = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer result() failed: {e:?}")))?;

        let s = v
            .to_string(&mut self.ctx)
            .map_err(|e| anyhow::anyhow!(format!("JS value to string error: {e:?}")))?;
        let out = s.to_std_string_escaped();
        let parsed = serde_json::from_str::<JsonValue>(&out).unwrap_or(JsonValue::Null);
        Ok(parsed)
    }
}

impl AnyTracer for JsTracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl EvmTracer for JsTracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        self.current_depth += 1;
        let obj = serde_json::json!({
            "from": request.caller(),
            "to": request.callee(),
            "type": format!("{:?}", request.modifier()),
            "value": if request.modifier() == CallModifier::Static {
                JsonValue::Null
            } else {
                serde_json::to_value(request.nominal_token_value()).unwrap_or(JsonValue::Null)
            },
            "input": Bytes::copy_from_slice(request.input()),
            "depth": self.current_depth,
        });

        _ = self.call_method("enter", &obj);
    }

    fn after_execution_frame_completed(&mut self, result: Option<(EvmResources, CallResult)>) {
        let (success, gas_used, output, revert_reason) = match result {
            Some((resources, res)) => match res {
                CallResult::Successful { returndata } => (
                    true,
                    U256::from(resources.ergs / ERGS_PER_GAS),
                    Some(Bytes::copy_from_slice(returndata)),
                    None,
                ),
                CallResult::Failed { returndata } => (
                    false,
                    U256::from(resources.ergs / ERGS_PER_GAS),
                    Some(Bytes::copy_from_slice(returndata)),
                    maybe_revert_reason(returndata),
                ),
            },
            None => (false, U256::ZERO, None, None),
        };
        let obj = serde_json::json!({
            "success": success,
            "gasUsed": gas_used,
            "output": output,
            "revertReason": revert_reason,
            "depth": self.current_depth,
        });
        let _ = self.call_method("exit", &obj);

        if self.current_depth > 0 {
            self.current_depth -= 1;
        }
    }

    fn on_storage_read(&mut self, _is_transient: bool, address: Address, key: B256, value: B256) {
        let obj = serde_json::json!({
            "address": address,
            "key": key,
            "value": value,
        });
        let _ = self.call_method("read", &obj);
    }

    fn on_storage_write(&mut self, _is_transient: bool, address: Address, key: B256, value: B256) {
        self.storage_overlay
            .borrow_mut()
            .insert((address, key), value);
        let obj = serde_json::json!({
            "address": address,
            "key": key,
            "value": value,
        });

        let _ = self.call_method("write", &obj);
    }

    fn on_bytecode_change(
        &mut self,
        address: Address,
        new_raw_bytecode: Option<&[u8]>,
        _new_internal_bytecode_hash: B256,
        new_observable_bytecode_length: u32,
    ) {
        if let Some(code) = new_raw_bytecode {
            let len = new_observable_bytecode_length as usize;
            let slice = if code.len() >= len {
                &code[..len]
            } else {
                code
            };
            let vec = slice.to_vec();
            self.code_overlay.borrow_mut().insert(address, vec.clone());
        }
        let obj = serde_json::json!({
            "address": address,
            "codeLen": new_observable_bytecode_length,
        });
        let _ = self.call_method("code", &obj);
    }

    fn on_event(&mut self, address: Address, topics: Vec<B256>, data: &[u8]) {
        let obj = serde_json::json!({
            "address": address,
            "topics": topics,
            "data": Bytes::copy_from_slice(data),
        });
        let _ = self.call_method("log", &obj);
    }

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_depth = 0;
        self.storage_overlay.borrow_mut().clear();
        self.code_overlay.borrow_mut().clear();

        // Call optional start(config)
        let config = self.tracer_config.clone();
        let _ = self.call_method("start", &config);
    }

    fn finish_tx(&mut self) {
        match self.call_result() {
            Ok(val) => self.results.push(val),
            Err(err) => {
                tracing::error!(?err, "JS tracer result() error; pushing null result");
                self.results.push(JsonValue::Null);
            }
        }
    }

    fn before_evm_interpreter_execution_step(
        &mut self,
        opcode: u8,
        _frame_state: impl EvmFrameInterface,
    ) {
        let obj = serde_json::json!({
            "op": opcode,
            "depth": self.current_depth,
        });
        let _ = self.call_method("step", &obj);
    }

    fn after_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        _frame_state: impl EvmFrameInterface,
    ) {
    }

    fn on_opcode_error(&mut self, error: &EvmError, _frame_state: impl EvmFrameInterface) {
        let obj = serde_json::json!({
            "error": fmt_error_msg(error),
            "depth": self.current_depth,
        });
        let _ = self.call_method("fault", &obj);
    }

    fn on_call_error(&mut self, error: &EvmError) {
        let obj = serde_json::json!({
            "error": fmt_error_msg(error),
            "depth": self.current_depth,
        });
        let _ = self.call_method("fault", &obj);
    }

    fn on_selfdestruct(
        &mut self,
        beneficiary: Address,
        token_value: U256,
        frame_state: impl EvmFrameInterface,
    ) {
        let obj = serde_json::json!({
            "address": frame_state.address(),
            "beneficiary": beneficiary,
            "value": token_value,
        });
        let _ = self.call_method("selfdestruct", &obj);
    }

    fn on_create_request(&mut self, _is_create2: bool) {}
}

pub fn trace_block<V: ViewState + 'static>(
    txs: Vec<ZkTransaction>,
    block_context: zksync_os_interface::types::BlockContext,
    state_view: V,
    js_tracer_config: JsonValue,
) -> anyhow::Result<Vec<JsonValue>> {
    let mut tracer = JsTracer::new(state_view.clone(), js_tracer_config)?;

    let tx_source = zksync_os_interface::traits::TxListSource {
        transactions: txs.into_iter().map(|tx| tx.encode()).collect(),
    };
    let _ = zksync_os_multivm::run_block(
        block_context,
        state_view.clone(),
        state_view,
        tx_source,
        zksync_os_interface::traits::NoopTxCallback,
        &mut tracer,
    )?;

    Ok(tracer.results)
}

fn extract_js_source_and_config(js_cfg: JsonValue) -> anyhow::Result<(String, JsonValue)> {
    let tracer_val = js_cfg
        .as_object()
        .unwrap_or(&Map::new())
        .get("tracer")
        .cloned()
        .unwrap_or(JsonValue::Null);

    let source = match tracer_val {
        JsonValue::String(s) => s,
        JsonValue::Object(map) => map
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };

    if source.is_empty() {
        return Err(anyhow::anyhow!(
            "JS tracer source not provided in 'tracer' field"
        ));
    }
    let config = js_cfg
        .get("config")
        .cloned()
        .or_else(|| js_cfg.get("tracerConfig").cloned())
        .unwrap_or(JsonValue::Null);

    Ok((source, config))
}

fn parse_address(s: &str) -> Option<Address> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::primitives::hex::decode(s).ok()?;
    if bytes.len() != 20 {
        return None;
    }

    Some(Address::from_slice(&bytes))
}

fn parse_b256(s: &str) -> Option<B256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::primitives::hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }

    Some(B256::from_slice(&bytes))
}

fn format_hex_u256(v: U256) -> String {
    if v == U256::ZERO {
        return "0x0".to_string();
    }

    format!("0x{v:x}")
}

fn anyhow_error_to_js_error(e: anyhow::Error) -> JsError {
    JsError::from_opaque(JsValue::from(JsString::from(e.to_string())))
}
