use crate::js_tracer::types::SelfdestructEntry;
use crate::js_tracer::{
    host::init_host_env_in_boa_context,
    types::{
        BalanceDelta, CreateType, FrameState, OverlayCheckpoint, OverlayEntry, OverlayState,
        StepCtx, TracerMethod, TxContext,
    },
    utils::{extract_js_source_and_config, gas_used_from_resources, wrap_js_invocation},
};
use crate::sandbox::{ERGS_PER_GAS, fmt_error_msg, maybe_revert_reason};
use alloy::hex::ToHexExt;
use alloy::primitives::{Address, B256, Bytes, U256};
use boa_engine::{Context as BoaContext, Source};
use serde_json::Value as JsonValue;
use std::ops::Not;
use std::{cell::RefCell, collections::hash_map::Entry};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources,
    EvmStackInterface, EvmTracer, NopValidator,
};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

const MAX_JS_TRACER_PAYLOAD_BYTES: usize = 512 * 1024;

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
/// - `ctx.gasPrice ` is not provided in result()
///
pub struct JsTracer {
    // JS runtime
    ctx: BoaContext,
    // User-provided tracer config
    tracer_config: JsonValue,

    // Overlays for storage and code modifications
    storage_overlay: OverlayState<(Address, B256), B256>,
    code_overlay: OverlayState<Address, Option<Vec<u8>>>,
    balance_overlay: OverlayState<Address, BalanceDelta>,
    selfdestruct_overlay: OverlayState<Address, SelfdestructEntry>,

    // Depth tracking and per-tx result
    current_depth: u64,
    pub(crate) results: Vec<JsonValue>,
    pending_step: Option<StepCtx>,
    pending_create_type: Option<CreateType>,

    frame_stack: Vec<FrameState>,
    last_finished_frame: Option<TxContext>,
    tx_failed: bool,

    error: Option<anyhow::Error>,
}

impl JsTracer {
    pub fn new(state_view: impl ViewState + 'static, js_cfg: String) -> anyhow::Result<Self> {
        if js_cfg.len() > MAX_JS_TRACER_PAYLOAD_BYTES {
            return Err(anyhow::anyhow!(format!(
                "JS tracer payload exceeds limit of {} bytes",
                MAX_JS_TRACER_PAYLOAD_BYTES
            )));
        }

        let (tracer_source, tracer_config) = extract_js_source_and_config(js_cfg)?;

        let mut ctx = BoaContext::default();

        let storage_overlay = OverlayState::<(Address, B256), B256>::new();
        let code_overlay = OverlayState::<Address, Option<Vec<u8>>>::new();
        let balance_overlay = OverlayState::<Address, BalanceDelta>::new();
        let selfdestruct_overlay = OverlayState::<Address, SelfdestructEntry>::new();

        init_host_env_in_boa_context(
            &mut ctx,
            &tracer_source,
            RefCell::new(state_view.clone()),
            storage_overlay.handle(),
            code_overlay.handle(),
            balance_overlay.handle(),
        )?;

        Ok(Self {
            ctx,
            tracer_config,
            storage_overlay,
            code_overlay,
            balance_overlay,
            selfdestruct_overlay,
            current_depth: 0,
            results: Vec::new(),
            pending_step: None,
            pending_create_type: None,
            error: None,
            frame_stack: Vec::new(),
            last_finished_frame: None,
            tx_failed: false,
        })
    }

    /// `call_method` invokes a method on the JS tracer object with the given argument.
    fn call_method(
        &mut self,
        method: TracerMethod,
        arg: &JsonValue,
        with_db: bool,
    ) -> anyhow::Result<()> {
        if !self.method_exists(method)? {
            return Ok(());
        }

        let method_name = method.as_str();
        let mut arg_json = serde_json::to_string(arg).unwrap_or("null".to_string());
        if with_db {
            arg_json = format!("{arg_json}, db");
        }
        let snippet = wrap_js_invocation(format!("tracer.{method_name}({arg_json});"));

        let _ = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| {
                anyhow::anyhow!(format!("JS tracer method {method_name} failed: {e:?}"))
            })?;

        Ok(())
    }

    fn method_exists(&mut self, method: TracerMethod) -> anyhow::Result<bool> {
        let method_name = method.as_str();
        Ok(self
            .ctx
            .eval(Source::from_bytes(
                format!(
                    "(function(){{ return typeof tracer === 'object' && typeof tracer.{method_name} === 'function' }})()"
                )
                    .as_bytes(),
            ))
            .map_err(|e| {
                anyhow::anyhow!(format!(
                    "JS tracer method existence check failed: {e:?}"
                ))
            })?
            .to_boolean())
    }

    fn commit_overlays(&self) {
        self.storage_overlay.commit();
        self.code_overlay.commit();
        self.balance_overlay.commit();
        self.selfdestruct_overlay.commit();
    }

    fn rollback_overlays(&self) {
        self.storage_overlay.rollback();
        self.code_overlay.rollback();
        self.balance_overlay.rollback();
        self.selfdestruct_overlay.rollback();
    }

    fn current_overlay_checkpoint(&self) -> OverlayCheckpoint {
        OverlayCheckpoint {
            storage: self.storage_overlay.checkpoint(),
            code: self.code_overlay.checkpoint(),
            balance: self.balance_overlay.checkpoint(),
            selfdestruct: self.selfdestruct_overlay.checkpoint(),
        }
    }

    fn clear_overlay_journals(&self) {
        self.storage_overlay.clear_journal();
        self.code_overlay.clear_journal();
        self.balance_overlay.clear_journal();
        self.selfdestruct_overlay.clear_journal();
    }

    fn revert_overlays_to_checkpoint(&self, checkpoint: OverlayCheckpoint) {
        self.storage_overlay
            .revert_to_checkpoint(checkpoint.storage);
        self.code_overlay.revert_to_checkpoint(checkpoint.code);
        self.balance_overlay
            .revert_to_checkpoint(checkpoint.balance);
        self.selfdestruct_overlay
            .revert_to_checkpoint(checkpoint.selfdestruct);
    }

    fn mark_contract_deployed(&self, address: Address) {
        if address == Address::ZERO {
            return;
        }

        let mut overlay = self.selfdestruct_overlay.borrow_mut();
        match overlay.entry(address) {
            Entry::Occupied(mut occupied) => {
                let before = occupied.get().clone();
                self.selfdestruct_overlay.record_update(address, before);
                occupied.get_mut().value.is_deployed_in_current_tx = true;
            }
            Entry::Vacant(vacant) => {
                self.selfdestruct_overlay.record_insert(address);
                vacant.insert(OverlayEntry::new_pending(SelfdestructEntry {
                    is_deployed_in_current_tx: true,
                    is_marked_for_selfdestruct: false,
                }));
            }
        }
    }

    fn apply_pending_selfdestructs(&mut self) {
        let entries: Vec<(Address, bool)> = self
            .selfdestruct_overlay
            .handle()
            .borrow()
            .iter()
            .map(|(address, entry)| {
                (
                    *address,
                    entry.value.is_marked_for_selfdestruct && entry.value.is_deployed_in_current_tx,
                )
            })
            .collect();

        for (address, should_destroy) in entries {
            if should_destroy {
                let keys: Vec<_> = self
                    .storage_overlay
                    .handle()
                    .borrow()
                    .keys()
                    .filter(|(addr, _)| *addr == address)
                    .cloned()
                    .collect();
                let mut storage_overlay = self.storage_overlay.borrow_mut();
                for key in keys {
                    storage_overlay.remove(&key);
                }

                self.code_overlay.borrow_mut().remove(&address);
                self.balance_overlay.borrow_mut().remove(&address);
            }

            self.selfdestruct_overlay.borrow_mut().remove(&address);
        }
    }

    fn call_enter(&mut self, call_frame: &JsonValue) -> anyhow::Result<()> {
        if !self.method_exists(TracerMethod::Enter)? {
            return Ok(());
        }

        let raw_frame_input = serde_json::to_string(call_frame).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;

        let method_name = TracerMethod::Enter.as_str();
        let body = format!(
            r#"
                let raw = {raw_frame_input};
                let frame = {{
                    getType() {{ return raw.type; }},
                    getFrom() {{ return raw.from; }},
                    getTo() {{ return raw.to; }},
                    getInput() {{ return hexToBytes(raw.input); }},
                    getGas() {{ return raw.gas; }},
                    getValue() {{ return raw.value; }},
                }};
                tracer.{method_name}(frame);
            "#
        );

        let _ = self
            .ctx
            .eval(Source::from_bytes(wrap_js_invocation(body).as_bytes()))
            .map_err(|e| {
                anyhow::anyhow!(format!("JS tracer method {method_name} failed: {e:?}"))
            })?;

        Ok(())
    }

    fn call_exit(&mut self, call_frame: &JsonValue) -> anyhow::Result<()> {
        if !self.method_exists(TracerMethod::Exit)? {
            return Ok(());
        }

        let raw_frame_input = serde_json::to_string(call_frame).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;

        let method_name = TracerMethod::Exit.as_str();
        let body = format!(
            r#"
                let raw = {raw_frame_input};
                let frame = {{
                    getGasUsed() {{ return raw.gasUsed; }},
                    getOutput() {{ return raw.output ? hexToBytes(raw.output) : null; }},
                    getError() {{ return raw.error; }},
                }};
                tracer.{method_name}(frame);
            "#
        );

        let _ = self
            .ctx
            .eval(Source::from_bytes(wrap_js_invocation(body).as_bytes()))
            .map_err(|e| {
                anyhow::anyhow!(format!("JS tracer method {method_name} failed: {e:?}"))
            })?;

        Ok(())
    }

    fn call_step_or_fault(
        &mut self,
        method: TracerMethod,
        raw_log: &JsonValue,
    ) -> anyhow::Result<()> {
        if !self.method_exists(method)? {
            return Ok(());
        }

        let raw_log_input = serde_json::to_string(raw_log).map_err(|e| {
            anyhow::anyhow!(format!("JS tracer log input serialization failed: {e:?}"))
        })?;
        let method_name = method.as_str();

        let has_error = raw_log
            .as_object()
            .and_then(|obj| obj.get("error"))
            .unwrap_or(&JsonValue::Null)
            .is_null()
            .not();

        let snippet = if has_error {
            format!(
                r#"
                    let raw = {raw_log_input};
                    let log = {{
                        getError() {{ return raw.error; }},
                        getDepth() {{ return raw.depth; }},
                    }};
                    tracer.{method_name}(log, db);
                "#
            )
        } else {
            format!(
                r#"
                    let raw = {raw_log_input};
                    let op = {{
                        toString() {{ return raw.op.name; }},
                        toNumber() {{ return raw.op.code; }},
                        isPush() {{ return raw.op.isPush; }},
                    }};
                    let memory = {{
                        __buffer: hexToBytes(raw.memory),
                        slice(start, stop) {{
                            const from = start >>> 0;
                            const to = stop === undefined ? this.__buffer.length : stop >>> 0;
                            return this.__buffer.slice(from, to);
                        }},
                        getUint(offset) {{
                            const from = offset >>> 0;
                            const end = from + 32;
                            const out = new Uint8Array(32);
                            const available = this.__buffer.slice(from, end);
                            out.set(available, 0);
                            return out;
                        }},
                        length() {{
                            return this.__buffer.length;
                        }},
                    }};
                    let contract = {{
                        __input: hexToBytes(raw.contract.input),
                        getCaller() {{ return raw.contract.caller; }},
                        getAddress() {{ return raw.contract.address; }},
                        getValue() {{ return raw.contract.value; }},
                        getInput() {{ return this.__input.slice(); }},
                    }};
                    let stack = {{
                        __entries: raw.stack,
                        length() {{ return this.__entries.length; }},
                        peek(n) {{ return this.__entries[n]; }},
                    }};
                    let log = {{
                        op,
                        memory,
                        contract,
                        stack,
                        getPC() {{ return raw.pc; }},
                        getGas() {{ return raw.gas; }},
                        getCost() {{ return raw.cost }},
                        getDepth() {{ return raw.depth; }},
                        getRefund() {{ return raw.refund; }},
                        getError() {{ return raw.error; }},
                    }};
                    tracer.{method_name}(log, db);
                "#
            )
        };

        let _ = self
            .ctx
            .eval(Source::from_bytes(wrap_js_invocation(snippet).as_bytes()))
            .map_err(|e| {
                anyhow::anyhow!(format!("JS tracer method {method_name} failed: {e:?}"))
            })?;

        Ok(())
    }

    fn invoke_method(&mut self, method: TracerMethod, arg: &JsonValue) {
        if self.error.is_some() {
            return;
        }

        if let Err(err) = match method {
            TracerMethod::Step | TracerMethod::Fault => self.call_step_or_fault(method, arg),
            TracerMethod::Setup | TracerMethod::Write => self.call_method(method, arg, false),
            TracerMethod::Enter => self.call_enter(arg),
            TracerMethod::Exit => self.call_exit(arg),
            TracerMethod::Result => self.call_method(method, arg, true),
            TracerMethod::StorageRead => Err(anyhow::anyhow!(
                "Storage read is not supported by JS tracer"
            )),
        } {
            self.record_error(method, err);
        }
    }

    fn record_error(&mut self, method: TracerMethod, err: anyhow::Error) {
        if self.error.is_none() {
            let method_name = method.as_str();
            tracing::debug!(
                ?err,
                method = method_name,
                "JS tracer execution halted due to error"
            );
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

        let method_name = TracerMethod::Result.as_str();
        let snippet = wrap_js_invocation(format!(
            "return JSON.stringify(tracer.{method_name}({ctx}, db));"
        ));
        let value = self
            .ctx
            .eval(Source::from_bytes(snippet.as_bytes()))
            .map_err(|e| {
                anyhow::anyhow!(format!("JS tracer method {method_name} failed: {e:?}"))
            })?;

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

        let stack = frame_state.stack();
        let mut stack_dump = Vec::with_capacity(stack.len());
        for idx in 0..stack.len() {
            match stack.peek_n(idx) {
                Ok(value) => stack_dump.push(format!("{value:066x}")),
                Err(err) => {
                    tracing::error!(?err, "Failed to read stack entry for JS tracer log");
                    break;
                }
            }
        }
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
            "stack": stack_dump,
            "depth": step_ctx.depth,
            "refund": frame_state.refund_counter(),
            "error": error,
        })
    }

    fn apply_balance_delta(
        &mut self,
        address: Address,
        credit: U256,
        debit: U256,
    ) -> anyhow::Result<()> {
        if credit == U256::ZERO && debit == U256::ZERO {
            return Ok(());
        }

        let mut overlay = self.balance_overlay.borrow_mut();
        match overlay.entry(address) {
            Entry::Occupied(mut occupied) => {
                let before = occupied.get().clone();
                self.balance_overlay.record_update(address, before);
                let entry = occupied.get_mut();
                if entry.committed && entry.previous.is_none() {
                    entry.previous = Some(entry.value.clone());
                }
                entry.value.credit(credit)?;
                entry.value.debit(debit)?;
                entry.committed = false;

                if entry.value.is_empty() && entry.previous.is_none() {
                    occupied.remove_entry();
                }
            }
            Entry::Vacant(vacant) => {
                let mut delta = BalanceDelta::default();
                delta.credit(credit)?;
                delta.debit(debit)?;
                if !delta.is_empty() {
                    self.balance_overlay.record_insert(address);
                    vacant.insert(OverlayEntry::new_pending(delta));
                }
            }
        }

        Ok(())
    }
}

impl AnyTracer for JsTracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl EvmTracer for JsTracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        let checkpoint = self.current_overlay_checkpoint();

        let call_value = request.nominal_token_value();
        if call_value != U256::ZERO {
            if let Err(err) = self.apply_balance_delta(request.caller(), U256::ZERO, call_value) {
                tracing::error!("Caller balance change failed on call enter: {:?}", err);
                self.record_error(TracerMethod::Enter, err);
                self.revert_overlays_to_checkpoint(checkpoint);
                return;
            }

            if let Err(err) = self.apply_balance_delta(request.callee(), call_value, U256::ZERO) {
                tracing::error!("Callee balance change failed on call enter: {:?}", err);
                self.record_error(TracerMethod::Enter, err);
                self.revert_overlays_to_checkpoint(checkpoint);
                return;
            }
        }

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
        self.frame_stack.push(FrameState {
            ctx: frame_ctx,
            checkpoint,
        });

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

        self.invoke_method(TracerMethod::Enter, &obj);
    }

    fn after_execution_frame_completed(&mut self, result: Option<(EvmResources, CallResult)>) {
        let (gas_used, output, revert_reason) = match &result {
            Some((resources, res)) => match res {
                CallResult::Successful { returndata } => (
                    gas_used_from_resources(resources.clone()),
                    Some(Bytes::copy_from_slice(returndata)),
                    None,
                ),
                CallResult::Failed { returndata } => (
                    gas_used_from_resources(resources.clone()),
                    Some(Bytes::copy_from_slice(returndata)),
                    maybe_revert_reason(returndata),
                ),
            },
            None => (U256::ZERO, None, None),
        };

        let frame_failed = matches!(result, Some((_, CallResult::Failed { .. })) | None);

        if let Some(mut frame_state) = self.frame_stack.pop() {
            let ctx = &mut frame_state.ctx;
            ctx.gas_used = Some(gas_used);
            ctx.output = output.clone();
            ctx.error = revert_reason.clone();

            if frame_failed {
                self.revert_overlays_to_checkpoint(frame_state.checkpoint);
            }

            if self.frame_stack.is_empty() && frame_failed {
                self.tx_failed = true;
            }

            self.last_finished_frame = Some(frame_state.ctx);
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
        self.invoke_method(TracerMethod::Exit, &obj);
    }

    /// This method only performs a sanity check that the values in the overlay match the ones
    /// from the state.
    fn on_storage_read(&mut self, _: bool, address: Address, key: B256, value: B256) {
        let storage_key = (address, key);
        if let Some(entry) = self
            .storage_overlay
            .handle()
            .borrow()
            .get(&storage_key)
            .cloned()
            && entry.value != value
        {
            tracing::error!(
                address = ?address,
                key = ?key,
                overlay_value = ?entry.value,
                actual_value = ?value,
                "Storage overlay/read mismatch"
            );
            self.record_error(
                TracerMethod::StorageRead,
                anyhow::anyhow!("Storage overlay value mismatch on read"),
            );
        }
    }

    fn on_storage_write(&mut self, _is_transient: bool, address: Address, key: B256, value: B256) {
        {
            let mut overlay = self.storage_overlay.borrow_mut();
            let storage_key = (address, key);
            match overlay.entry(storage_key) {
                Entry::Occupied(mut entry) => {
                    let before = entry.get().clone();
                    self.storage_overlay.record_update(storage_key, before);
                    let slot = entry.get_mut();
                    slot.previous = Some(slot.value);
                    slot.value = value;
                    slot.committed = false;
                }
                Entry::Vacant(vacant) => {
                    self.storage_overlay.record_insert(storage_key);
                    vacant.insert(OverlayEntry::new_pending(value));
                }
            }
        }
        let obj = serde_json::json!({
            "address": address,
            "key": key,
            "value": value,
        });

        // this method is an extension beyond geth tracer interface, convenient for state change tracking
        self.invoke_method(TracerMethod::Write, &obj);
    }

    fn on_bytecode_change(
        &mut self,
        address: Address,
        new_raw_bytecode: Option<&[u8]>,
        _new_internal_bytecode_hash: B256,
        new_observable_bytecode_length: u32,
    ) {
        let new_value = new_raw_bytecode.map(|code| {
            let len = new_observable_bytecode_length as usize;
            let slice = if code.len() >= len {
                &code[..len]
            } else {
                code
            };
            slice.to_vec()
        });

        if new_value.is_some() {
            self.mark_contract_deployed(address);
        }

        let mut overlay = self.code_overlay.borrow_mut();
        match overlay.entry(address) {
            Entry::Occupied(mut entry) => {
                let before = entry.get().clone();
                self.code_overlay.record_update(address, before);
                let record = entry.get_mut();
                if record.committed && record.previous.is_none() {
                    record.previous = Some(record.value.clone());
                }
                record.value = new_value.clone();
                record.committed = false;
            }
            Entry::Vacant(vacant) => {
                self.code_overlay.record_insert(address);
                vacant.insert(OverlayEntry::new_pending(new_value));
            }
        }
    }

    fn on_event(&mut self, _: Address, _: Vec<B256>, _: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.tx_failed = false;
        self.current_depth = 0;
        self.pending_step = None;
        self.pending_create_type = None;
        self.last_finished_frame = None;
        self.frame_stack.clear();
        self.clear_overlay_journals();

        let config = self.tracer_config.clone();
        self.invoke_method(TracerMethod::Setup, &config);
    }

    fn finish_tx(&mut self) {
        if self.error.is_some() {
            self.rollback_overlays();
            self.clear_overlay_journals();
            self.frame_stack.clear();
            self.tx_failed = false;
            self.last_finished_frame = None;
            return;
        }

        let ctx = match self.last_finished_frame.clone() {
            Some(frame) => frame,
            None => {
                tracing::error!("No finished frame found at transaction end");
                self.record_error(
                    TracerMethod::Result,
                    anyhow::anyhow!("No finished frame found at transaction end"),
                );
                self.rollback_overlays();
                self.clear_overlay_journals();
                self.frame_stack.clear();
                self.tx_failed = false;
                self.last_finished_frame = None;

                return;
            }
        };
        self.pending_step = None;

        let mut tx_failed = self.tx_failed || ctx.error.is_some();

        match self.call_result(&ctx) {
            Ok(val) => self.results.push(val),
            Err(err) => {
                tx_failed = true;
                self.record_error(TracerMethod::Result, err);
            }
        }

        if tx_failed {
            self.rollback_overlays();
        } else {
            self.apply_pending_selfdestructs();
            self.commit_overlays();
        }

        self.clear_overlay_journals();
        self.frame_stack.clear();
        self.tx_failed = false;
        self.last_finished_frame = None;
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
        self.invoke_method(TracerMethod::Step, &log);
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

        self.invoke_method(TracerMethod::Fault, &log);
    }

    fn on_call_error(&mut self, error: &EvmError) {
        self.pending_step = None;
        self.tx_failed = true;
        let obj = serde_json::json!({
            "error": fmt_error_msg(error),
            "depth": self.current_depth,
        });

        self.invoke_method(TracerMethod::Fault, &obj);
    }

    fn on_selfdestruct(
        &mut self,
        beneficiary: Address,
        token_value: U256,
        frame_state: impl EvmFrameInterface,
    ) {
        if token_value != U256::ZERO {
            if let Err(err) =
                self.apply_balance_delta(frame_state.address(), U256::ZERO, token_value)
            {
                tracing::error!("Selfdestruct balance debit failed: {:?}", err);
                self.record_error(TracerMethod::Enter, err);
            }

            if let Err(err) = self.apply_balance_delta(beneficiary, token_value, U256::ZERO) {
                tracing::error!("Selfdestruct beneficiary credit failed: {:?}", err);
                self.record_error(TracerMethod::Enter, err);
            }
        }

        let address = frame_state.address();
        let mut overlay = self.selfdestruct_overlay.borrow_mut();
        match overlay.entry(address) {
            Entry::Occupied(mut entry) => {
                let before = entry.get().clone();
                self.selfdestruct_overlay.record_update(address, before);
                entry.get_mut().value.is_marked_for_selfdestruct = true;
            }
            Entry::Vacant(vacant) => {
                self.selfdestruct_overlay.record_insert(address);
                vacant.insert(OverlayEntry::new_pending(SelfdestructEntry {
                    is_deployed_in_current_tx: false,
                    is_marked_for_selfdestruct: true,
                }));
            }
        }
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
    js_tracer_config: String,
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
        &mut NopValidator,
    )?;

    if let Some(err) = tracer.take_error() {
        return Err(err);
    }

    Ok(tracer.results)
}
