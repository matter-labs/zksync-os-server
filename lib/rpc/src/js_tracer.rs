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

#[derive(Clone, Copy, Debug)]
enum CreateType {
    Create,
    Create2,
}

#[allow(clippy::enum_variant_names)]
enum HostMethod {
    GetBalance,
    GetNonce,
    GetCode,
    GetState,
}

impl HostMethod {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "getBalance" => Some(Self::GetBalance),
            "getNonce" => Some(Self::GetNonce),
            "getCode" => Some(Self::GetCode),
            "getState" => Some(Self::GetState),
            _ => None,
        }
    }
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
    pending_create_type: Option<CreateType>,
    error: Option<anyhow::Error>,
}

impl JsTracer {
    pub fn new(state_view: impl ViewState + 'static, js_cfg: JsonValue) -> anyhow::Result<Self> {
        let (tracer_source, tracer_config) = extract_js_source_and_config(js_cfg)?;

        let mut ctx = BoaContext::default();
        bootstrap_tracer(&mut ctx, &tracer_source)?;

        // Prepare shared state for DB methods
        let storage_overlay = Rc::new(RefCell::new(HashMap::<(Address, B256), B256>::new()));
        let code_overlay = Rc::new(RefCell::new(HashMap::new()));

        let host_env = HostEnvironment {
            state_view: RefCell::new(state_view.clone()),
            storage_overlay: Rc::clone(&storage_overlay),
            code_overlay: Rc::clone(&code_overlay),
        };
        install_host_bindings(&mut ctx, host_env)?;
        install_db_wrapper(&mut ctx)?;

        Ok(Self {
            ctx,
            tracer_config,
            storage_overlay,
            code_overlay,
            current_depth: 0,
            results: Vec::new(),
            pending_create_type: None,
            error: None,
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

    fn invoke_method(&mut self, method: &str, arg: &JsonValue) {
        if let Err(err) = self.call_method(method, arg) {
            self.record_error(err);
        }
    }

    fn record_error(&mut self, err: anyhow::Error) {
        if self.error.is_none() {
            tracing::debug!(?err, "JS tracer execution halted due to error");
            self.error = Some(err);
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
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

    fn consume_call_type(&mut self, modifier: CallModifier) -> String {
        let typ = match modifier {
            CallModifier::NoModifier => "CALL".to_string(),
            CallModifier::Constructor => match self
                .pending_create_type
                .take()
                .unwrap_or(CreateType::Create)
            {
                CreateType::Create => "CREATE".to_string(),
                CreateType::Create2 => "CREATE2".to_string(),
            },
            CallModifier::Delegate | CallModifier::DelegateStatic => "DELEGATECALL".to_string(),
            CallModifier::Static => "STATICCALL".to_string(),
            CallModifier::EVMCallcode | CallModifier::EVMCallcodeStatic => "CALLCODE".to_string(),
            CallModifier::ZKVMSystem | CallModifier::ZKVMSystemStatic => {
                panic!("unexpected call type: {modifier:?}")
            }
        };

        if self.pending_create_type.is_some() {
            self.pending_create_type = None;
        }

        typ
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
        if self.current_depth == 1 && request.modifier() == CallModifier::Constructor {
            self.pending_create_type = Some(CreateType::Create);
        }

        let call_type = self.consume_call_type(request.modifier());
        let gas = U256::from(request.resources().ergs / ERGS_PER_GAS);
        let obj = serde_json::json!({
            "type": call_type,
            "from": request.caller(),
            "to": request.callee(),
            "gas": gas,
            "value": match request.modifier() {
                CallModifier::Static => JsonValue::Null,
                _ => serde_json::to_value(request.nominal_token_value()).unwrap_or(JsonValue::Null),
            },
            "input": Bytes::copy_from_slice(request.input()),
            "depth": self.current_depth,
        });

        self.invoke_method("enter", &obj);
    }

    fn after_execution_frame_completed(&mut self, result: Option<(EvmResources, CallResult)>) {
        let (gas_used, output, revert_reason) = match result {
            Some((resources, res)) => match res {
                CallResult::Successful { returndata } => (
                    gas_used_from_resources(resources),
                    Some(Bytes::copy_from_slice(returndata)),
                    None,
                ),
                CallResult::Failed { returndata } => (
                    gas_used_from_resources(resources),
                    Some(Bytes::copy_from_slice(returndata)),
                    maybe_revert_reason(returndata),
                ),
            },
            None => (U256::ZERO, None, None),
        };
        let obj = serde_json::json!({
            "gasUsed": gas_used,
            "output": output,
            "revertReason": revert_reason,
            "error": JsonValue::Null,
        });
        self.invoke_method("exit", &obj);

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
        self.invoke_method("read", &obj);
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

        self.invoke_method("write", &obj);
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
            let obj = serde_json::json!({
                "address": address,
                "code": Bytes::from(vec),
            });
            self.invoke_method("code", &obj);
        } else {
            self.code_overlay.borrow_mut().remove(&address);
            let obj = serde_json::json!({
                "address": address,
                "code": JsonValue::Null,
            });
            self.invoke_method("code", &obj);
        }
    }

    fn on_event(&mut self, address: Address, topics: Vec<B256>, data: &[u8]) {
        let obj = serde_json::json!({
            "address": address,
            "topics": topics,
            "data": Bytes::copy_from_slice(data),
        });
        self.invoke_method("log", &obj);
    }

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_depth = 0;
        self.pending_create_type = None;

        // Call optional start(config)
        let config = self.tracer_config.clone();
        self.invoke_method("start", &config);
    }

    fn finish_tx(&mut self) {
        if self.error.is_some() {
            return;
        }

        match self.call_result() {
            Ok(val) => self.results.push(val),
            Err(err) => self.record_error(err),
        }
    }

    fn before_evm_interpreter_execution_step(
        &mut self,
        opcode: u8,
        _frame_state: impl EvmFrameInterface,
    ) {
        let obj = serde_json::json!({
            "op": zk_os_evm_interpreter::opcodes::OPCODE_JUMPMAP[opcode as usize].unwrap_or("Invalid opcode"),
            "depth": self.current_depth,
        });
        self.invoke_method("step", &obj);
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
        self.invoke_method("fault", &obj);
    }

    fn on_call_error(&mut self, error: &EvmError) {
        let obj = serde_json::json!({
            "error": fmt_error_msg(error),
            "depth": self.current_depth,
        });
        self.invoke_method("fault", &obj);
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
            "balance": token_value,
        });
        self.invoke_method("selfdestruct", &obj);
    }

    fn on_create_request(&mut self, is_create2: bool) {
        self.pending_create_type = Some(if is_create2 {
            CreateType::Create2
        } else {
            CreateType::Create
        });
    }
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

    if let Some(err) = tracer.take_error() {
        return Err(err);
    }

    Ok(tracer.results)
}

fn bootstrap_tracer(ctx: &mut BoaContext, tracer_source: &str) -> anyhow::Result<()> {
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

    Ok(())
}

fn install_host_bindings<V: ViewState + 'static>(
    ctx: &mut BoaContext,
    env: HostEnvironment<V>,
) -> anyhow::Result<()> {
    let host = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |_this, args, env, ctx| {
                let method_name = args
                    .get_or_undefined(0)
                    .to_string(ctx)?
                    .to_std_string_escaped();

                let payload_raw = args
                    .get_or_undefined(1)
                    .to_string(ctx)?
                    .to_std_string_escaped();

                let payload: JsonValue = serde_json::from_str(&payload_raw)
                    .map_err(|err| anyhow_error_to_js_error(anyhow::anyhow!(err)))?;

                let Some(method) = HostMethod::parse(&method_name) else {
                    return Ok(JsValue::from(js_string!("null")));
                };

                let response =
                    dispatch_host_call(env, method, &payload).map_err(anyhow_error_to_js_error)?;

                Ok(JsValue::from(js_string!(response)))
            },
            env,
        ),
    )
    .name(js_string!("__hostCall"))
    .length(2)
    .build();

    ctx.global_object()
        .set(js_string!("__hostCall"), host, false, ctx)
        .map_err(|e| anyhow::anyhow!(format!("install __hostCall failed: {e:?}")))?;

    Ok(())
}

fn install_db_wrapper(ctx: &mut BoaContext) -> anyhow::Result<()> {
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

    Ok(())
}

fn dispatch_host_call<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    method: HostMethod,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    match method {
        HostMethod::GetBalance => host_get_balance(env, payload),
        HostMethod::GetNonce => host_get_nonce(env, payload),
        HostMethod::GetCode => host_get_code(env, payload),
        HostMethod::GetState => host_get_state(env, payload),
    }
}

fn host_get_balance<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x0".to_string());
    };

    let balance = env
        .state_view
        .borrow_mut()
        .get_account(address)
        .ok_or(anyhow::anyhow!("Account {address:?} not found in a state"))?
        .balance;

    Ok(format_hex_u256(balance))
}

fn host_get_nonce<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x0".to_string());
    };

    let nonce = env
        .state_view
        .borrow_mut()
        .account_nonce(address)
        .ok_or(anyhow::anyhow!("Account {address:?} not found in a state"))?;

    Ok(format!("0x{nonce:x}"))
}

fn host_get_code<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x".to_string());
    };

    if let Some(code) = env.code_overlay.borrow().get(&address) {
        return Ok(format!("0x{}", alloy::primitives::hex::encode(code)));
    }

    let code = {
        let mut state_view = env.state_view.borrow_mut();
        let props = state_view
            .get_account(address)
            .ok_or(anyhow::anyhow!("Account {address:?} not found in a state"))?;
        get_code(&mut *state_view, &props)
    };

    Ok(format!("0x{}", alloy::primitives::hex::encode(code)))
}

fn host_get_state<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("address is not supplied in getState"))?;
    let slot = payload
        .get("slot")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("slot is not supplied in getState"))?;

    let (Some(address), Some(key)) = (parse_address(addr), parse_b256(slot)) else {
        return Ok("0x0".to_string());
    };

    if let Some(value) = env.storage_overlay.borrow().get(&(address, key)) {
        return Ok(format!("0x{}", alloy::primitives::hex::encode(value.0)));
    }

    let flat = derive_flat_storage_key(&B160::from_be_bytes(address.into_array()), &(key.0.into()));
    let value = env
        .state_view
        .borrow_mut()
        .read(B256::from(flat.as_u8_array()))
        .unwrap_or_default();

    Ok(format!("0x{}", alloy::primitives::hex::encode(value.0)))
}

fn gas_used_from_resources(resources: EvmResources) -> U256 {
    U256::from(resources.ergs / ERGS_PER_GAS)
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
