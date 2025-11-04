use crate::js_tracer::types::TxContext;
use crate::js_tracer::{
    host::init_host_env_in_boa_context,
    types::{CreateType, StepCtx},
    utils::{extract_js_source_and_config, gas_used_from_resources},
};
use crate::sandbox::{ERGS_PER_GAS, fmt_error_msg, maybe_revert_reason};
use alloy::hex::ToHexExt;
use alloy::primitives::{Address, B256, Bytes, U256};
use boa_engine::{Context as BoaContext, Source};
use serde_json::Value as JsonValue;
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmTracer,
};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

/// JS tracer implementation
/// Holds a Boa JS runtime and calls user-provided JS tracer methods when the hooks of zksync-os
/// EVM tracer interface are invoked.
/// Since zksync-os interfaces don't provide state access - we use the state before the execution of
/// each transaction and maintain overlays for storage and code modifications done during the tx.
///
/// Tracer methods supported:
/// - setup(config): called once at the beginning of the transaction with the tracer config
/// - enter(frame): called on entering a new execution frame
/// - exit(result): called on exiting an execution frame
/// - step(log, db): called after each EVM opcode execution step
/// - fault(log, db): called on EVM opcode error
/// - result(ctx, db): called at the end of the transaction to get the final result
/// - write(modification): called on each storage write (extension beyond geth tracer)
///
/// The JS tracer can use the `db` object to query the state via the following interface:
///   - getBalance(address): returns balance of an address
///   - getNonce(address): returns the nonce as hex string
///   - getCode(address): returns code at address
///   - getState(address, slot): returns storage value at slot
///   - exists(address): returns true if the address exists in the state or overlays
///
/// Known divergences from geth tracer interface:
/// - `stack` is not provided in step/fault logs
/// - `ctx.gasPrice ` is not provided in result()
///
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
    pub(crate) results: Vec<JsonValue>,
    pending_step: Option<StepCtx>,
    pending_create_type: Option<CreateType>,

    frame_stack: Vec<TxContext>,
    last_finished_frame: Option<TxContext>,

    error: Option<anyhow::Error>,
}

impl JsTracer {
    pub fn new(state_view: impl ViewState + 'static, js_cfg: JsonValue) -> anyhow::Result<Self> {
        let (tracer_source, tracer_config) = extract_js_source_and_config(js_cfg)?;

        let mut ctx = BoaContext::default();

        let storage_overlay = Rc::new(RefCell::new(HashMap::<(Address, B256), B256>::new()));
        let code_overlay = Rc::new(RefCell::new(HashMap::new()));

        init_host_env_in_boa_context(
            &mut ctx,
            &tracer_source,
            RefCell::new(state_view.clone()),
            Rc::clone(&storage_overlay),
            Rc::clone(&code_overlay),
        )?;

        Ok(Self {
            ctx,
            tracer_config,
            storage_overlay,
            code_overlay,
            current_depth: 0,
            results: Vec::new(),
            pending_step: None,
            pending_create_type: None,
            error: None,
            frame_stack: Vec::new(),
            last_finished_frame: None,
        })
    }

    /// `call_method` invokes a method on the JS tracer object with the given argument.
    fn call_method(&mut self, method: &str, arg: &JsonValue, with_db: bool) -> anyhow::Result<()> {
        if !self.method_exists(method)? {
            return Ok(());
        }

        let mut arg_json = serde_json::to_string(arg).unwrap_or("null".to_string());
        if with_db {
            arg_json = format!("{arg_json}, db");
        }
        let snippet = format!("(function(){{ tracer.{method}({arg_json}) }})()");

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method {method} failed: {e:?}")))?;

        Ok(())
    }

    fn method_exists(&mut self, method: &str) -> anyhow::Result<bool> {
        Ok(self.ctx.eval(Source::from_bytes(
            format!(
                "(function(){{ return typeof tracer === 'object' && typeof tracer.{method} === 'function' }})()"
            ).as_bytes()
        ))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method existence check failed: {e:?}")))?
            .to_boolean())
    }

    fn call_enter(&mut self, call_frame: &JsonValue) -> anyhow::Result<()> {
        if !self.method_exists("enter")? {
            return Ok(());
        }

        let raw_frame_input = serde_json::to_string(call_frame).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;

        let snippet = format!(
            "(function(){{\n\
                let raw = {raw_frame_input};\n\
                let frame = {{\n\
                    getType(){{ return raw.type; }},\n\
                    getFrom(){{ return raw.from; }},\n\
                    getTo(){{ return raw.to; }},\n\
                    getInput(){{ return hexToBytes(raw.input); }},\n\
                    getGas(){{ return raw.gas; }},\n\
                    getValue(){{ return raw.value; }},\n\
                }};\n\
                tracer.enter(frame);\n\
            }})()"
        );

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method enter failed: {e:?}")))?;

        Ok(())
    }

    fn call_exit(&mut self, call_frame: &JsonValue) -> anyhow::Result<()> {
        if !self.method_exists("exit")? {
            return Ok(());
        }

        let raw_frame_input = serde_json::to_string(call_frame).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;

        let snippet = format!(
            "(function(){{\n\
                let raw = {raw_frame_input};\n\
                let frame = {{\n\
                    getGasUsed(){{ return raw.gasUsed; }},\n\
                    getOutput(){{ return raw.output ? hexToBytes(raw.output) : null; }},\n\
                    getError(){{ return raw.error; }},\n\
                }};\n\
                tracer.exit(frame);\n\
            }})()"
        );

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method exit failed: {e:?}")))?;

        Ok(())
    }

    fn call_step_or_fault(&mut self, method: &str, raw_log: &JsonValue) -> anyhow::Result<()> {
        if !self.method_exists(method)? {
            return Ok(());
        }

        let raw_log_input = serde_json::to_string(raw_log).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;

        let has_error = raw_log
            .as_object()
            .map(|o| o.contains_key("error"))
            .unwrap_or(false);

        let snippet = if has_error {
            format!(
                "(function(){{\n\
                let raw = {raw_log_input};\n\
                let log = {{ getError(){{ return raw.error; }}, getDepth(){{ return raw.depth; }} }};\n\
                tracer.{method}(log, db);\n\
            }})()"
            )
        } else {
            format!(
                "(function(){{\n\
                let raw = {raw_log_input};\n\
                let op = {{
                    toString(){{ return raw.op.name; }},
                    toNumber(){{ return raw.op.code; }},
                    isPush(){{ return raw.op.isPush; }}
                }};\n\
                let memory = {{
                    __buffer: hexToBytes(raw.memory),
                    slice(start, stop){{
                        const from = start >>> 0;
                        const to = stop === undefined ? this.__buffer.length : stop >>> 0;
                        return this.__buffer.slice(from, to);
                    }},
                    getUint(offset){{
                        const from = offset >>> 0;
                        const end = from + 32;
                        const out = new Uint8Array(32);
                        const available = this.__buffer.slice(from, end);
                        out.set(available, 0);
                        return out;
                    }},
                    length(){{
                        return this.__buffer.length;
                    }}
                }};\n\
                let contract = {{
                    __input: hexToBytes(raw.contract.input),
                    getCaller(){{ return raw.contract.caller; }},
                    getAddress(){{ return raw.contract.address; }},
                    getValue(){{ return raw.contract.value; }},
                    getInput(){{ return this.__input.slice(); }}
                }};\n\
                let log = {{
                    op: op,
                    memory: memory,
                    contract: contract,
                    getPC(){{ return raw.pc; }},
                    getGas(){{ return raw.gas; }},
                    getCost(){{ return raw.cost }},
                    getDepth(){{ return raw.depth; }},
                    getRefund(){{ return raw.refund; }},
                    getError(){{ return raw.error; }}
                }};\n\
                tracer.{method}(log, db);\n\
            }})()"
            )
        };

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer method {method} failed: {e:?}")))?;

        Ok(())
    }

    fn invoke_method(&mut self, method: &str, arg: &JsonValue) {
        if let Err(err) = match method {
            "step" | "fault" => self.call_step_or_fault(method, arg),
            "setup" | "write" => self.call_method(method, arg, false),
            "enter" => self.call_enter(arg),
            "exit" => self.call_exit(arg),
            "result" => self.call_method(method, arg, true),
            _ => Err(anyhow::anyhow!(format!(
                "unknown JS tracer method: {method}"
            ))),
        } {
            self.record_error(method, err);
        }
    }

    fn record_error(&mut self, method: &str, err: anyhow::Error) {
        if self.error.is_none() {
            tracing::debug!(?err, ?method, "JS tracer execution halted due to error");
            self.error = Some(err);
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }

    /// `call_result` is called at the end of the transaction to get the final result from the tracer.
    fn call_result(&mut self, ctx: &TxContext) -> anyhow::Result<JsonValue> {
        let ctx = serde_json::json!({
            "type": ctx.typ,
            "from": ctx.from,
            "to": ctx.to,
            "input": ctx.input,
            "gas": ctx.gas,
            "value": match ctx.value {
                v if v == U256::ZERO => JsonValue::Null,
                v => serde_json::to_value(v).unwrap_or(JsonValue::Null),
            },
            "gasUsed": ctx.gas_used,
            "output": ctx.output,
            "error": ctx.error,
        });

        let snippet =
            format!("(function(){{ return JSON.stringify(tracer.result({ctx}, db)); }})()");
        let value = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| anyhow::anyhow!(format!("JS tracer result() failed: {e:?}")))?;

        let out = value
            .to_string(&mut self.ctx)
            .map_err(|e| anyhow::anyhow!(format!("JS value to string error: {e:?}")))?
            .to_std_string_escaped();

        Ok(serde_json::from_str::<JsonValue>(&out).unwrap_or(JsonValue::Null))
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

    fn prepare_log_input(
        &mut self,
        step_ctx: StepCtx,
        frame_state: &impl EvmFrameInterface,
        error: Option<String>,
    ) -> serde_json::Value {
        let gas_after = frame_state.resources().ergs / ERGS_PER_GAS;
        let cost = step_ctx.gas_before.saturating_sub(gas_after);

        let memory_bytes = Bytes::copy_from_slice(frame_state.heap());
        let contract_input = Bytes::copy_from_slice(frame_state.calldata());
        let contract_value = format!("{:#x}", frame_state.call_value());

        let opcode_name = zk_os_evm_interpreter::opcodes::OPCODE_JUMPMAP[step_ctx.opcode as usize]
            .unwrap_or("Invalid opcode");
        let is_push = opcode_name.starts_with("PUSH");

        serde_json::json!({
            "op": {
                "name": opcode_name,
                "code": step_ctx.opcode,
                "isPush": is_push,
            },
            "memory": memory_bytes,
            "contract": {
                "caller": frame_state.caller(),
                "address": frame_state.address(),
                "value": contract_value,
                "input": contract_input,
            },
            "pc": step_ctx.pc,
            "gas": step_ctx.gas_before,
            "cost": cost,
            "depth": step_ctx.depth,
            "refund": frame_state.refund_counter(),
            "error": error,
        })
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
        let input = Bytes::copy_from_slice(request.input());
        let frame_ctx = TxContext {
            typ: call_type.clone(),
            from: request.caller(),
            to: request.callee(),
            input: input.clone(),
            gas,
            value: match request.modifier() {
                CallModifier::Static => U256::ZERO,
                _ => request.nominal_token_value(),
            },
            gas_used: None,
            output: None,
            error: None,
        };
        self.frame_stack.push(frame_ctx);

        let obj = serde_json::json!({
            "type": call_type,
            "from": request.caller(),
            "to": request.callee(),
            "input": input.encode_hex(),
            "gas": gas,
            "value": match request.modifier() {
                CallModifier::Static => JsonValue::Null,
                _ => serde_json::to_value(request.nominal_token_value()).unwrap_or(JsonValue::Null),
            },
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

        if let Some(ctx) = &mut self.frame_stack.pop() {
            ctx.gas_used = Some(gas_used);
            ctx.output = output.clone();
            ctx.error = revert_reason.clone();
            self.last_finished_frame = Some(ctx.clone());
        } else {
            tracing::error!("Execution frame completed but no frame context found");
        }

        if self.current_depth > 0 {
            self.current_depth -= 1;
        }

        let obj = serde_json::json!({
            "gasUsed": gas_used,
            "output": output.map(|o| o.encode_hex()),
            "error": revert_reason
        });
        self.invoke_method("exit", &obj);
    }

    fn on_storage_read(&mut self, _: bool, _: Address, _: B256, _: B256) {}

    fn on_storage_write(&mut self, _is_transient: bool, address: Address, key: B256, value: B256) {
        self.storage_overlay
            .borrow_mut()
            .insert((address, key), value);
        let obj = serde_json::json!({
            "address": address,
            "key": key,
            "value": value,
        });

        // this method is an extension beyond geth tracer interface, convenient for state change tracking
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
        } else {
            self.code_overlay.borrow_mut().remove(&address);
        }
    }

    fn on_event(&mut self, _: Address, _: Vec<B256>, _: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_depth = 0;
        self.pending_step = None;
        self.pending_create_type = None;

        let config = self.tracer_config.clone();
        self.invoke_method("setup", &config);
    }

    fn finish_tx(&mut self) {
        if self.error.is_some() {
            return;
        }

        let ctx = match self.last_finished_frame.clone() {
            Some(frame) => frame,
            None => {
                tracing::error!("No finished frame found at transaction end");
                self.record_error(
                    "result",
                    anyhow::anyhow!("No finished frame found at transaction end"),
                );

                return;
            }
        };
        self.pending_step = None;

        match self.call_result(&ctx) {
            Ok(val) => self.results.push(val),
            Err(err) => self.record_error("result", err),
        }
    }

    fn before_evm_interpreter_execution_step(
        &mut self,
        opcode: u8,
        frame_state: impl EvmFrameInterface,
    ) {
        let gas_before = frame_state.resources().ergs / ERGS_PER_GAS;
        let pc = frame_state.instruction_pointer() as u64;

        self.pending_step = Some(StepCtx {
            opcode,
            pc,
            gas_before,
            depth: self.current_depth,
        });
    }

    fn after_evm_interpreter_execution_step(
        &mut self,
        opcode: u8,
        frame_state: impl EvmFrameInterface,
    ) {
        let pending = self.pending_step.take().unwrap_or_else(|| StepCtx {
            opcode,
            pc: frame_state.instruction_pointer() as u64,
            gas_before: frame_state.resources().ergs / ERGS_PER_GAS,
            depth: self.current_depth,
        });

        let log = self.prepare_log_input(pending, &frame_state, None);
        self.invoke_method("step", &log);
    }

    fn on_opcode_error(&mut self, error: &EvmError, frame_state: impl EvmFrameInterface) {
        let message = fmt_error_msg(error);
        let log = if let Some(pending) = self.pending_step.take() {
            self.prepare_log_input(pending, &frame_state, Some(message.clone()))
        } else {
            tracing::error!("Received opcode error without pending step context");
            serde_json::json!({
                "error": message,
                "depth": self.current_depth,
            })
        };

        self.invoke_method("fault", &log);
    }

    fn on_call_error(&mut self, error: &EvmError) {
        self.pending_step = None;
        let obj = serde_json::json!({
            "error": fmt_error_msg(error),
            "depth": self.current_depth,
        });

        self.invoke_method("fault", &obj);
    }

    fn on_selfdestruct(&mut self, _: Address, _: U256, _: impl EvmFrameInterface) {}

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
