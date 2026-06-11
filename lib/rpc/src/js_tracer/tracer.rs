use crate::js_tracer::types::SelfdestructEntry;
use crate::js_tracer::{
    host::init_host_env_in_boa_context,
    types::{
        BalanceDelta, CreateType, FrameState, OverlayCheckpoint, OverlayEntry, OverlayState,
        StepCtx, TracerMethod, TxContext,
    },
    utils::{extract_js_source_and_config, gas_used_from_resources},
};
use crate::sandbox::{ERGS_PER_GAS, fmt_error_msg, maybe_revert_reason};
use alloy::hex::ToHexExt;
use alloy::primitives::{Address, B256, Bytes, U256};
use boa_engine::{Context as BoaContext, JsValue, Source, js_string, object::JsObject};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::hash_map::Entry};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources,
    EvmStackInterface, EvmTracer, NopValidator,
};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

const MAX_JS_TRACER_PAYLOAD_BYTES: usize = 512 * 1024;

/// Wall-clock ceiling for a single tracer's total execution. Checked between EVM opcode steps; if
/// exceeded the tracer aborts with an error instead of running unbounded. This bounds the
/// "millions of cheap steps, each doing some JS work" case (and any genuinely long trace), so a
/// `debug_trace*` request can no longer pin a blocking worker indefinitely.
const JS_TRACER_EXECUTION_DEADLINE: Duration = Duration::from_secs(30);

/// Per-invocation cap on JS loop iterations (Boa runtime limit). Boa's counter is reset per
/// top-level call, so this bounds an infinite/runaway loop *inside a single* hook (e.g.
/// `while (true) {}` in `step`/`result`) — the one runaway the wall-clock deadline can't catch,
/// since control never returns to the Rust side mid-loop. Set generously so legitimate tracers
/// iterating over collected per-step data are unaffected.
const JS_TRACER_MAX_LOOP_ITERATIONS: u64 = 50_000_000;

// Names of the per-hook invoker functions installed once in `host::install_invocation_helpers`.
const INVOKE_SETUP: &str = "__zkjs_invoke_setup";
const INVOKE_STEP: &str = "__zkjs_invoke_step";
const INVOKE_STEP_ERR: &str = "__zkjs_invoke_step_err";
const INVOKE_FAULT: &str = "__zkjs_invoke_fault";
const INVOKE_FAULT_ERR: &str = "__zkjs_invoke_fault_err";
const INVOKE_ENTER: &str = "__zkjs_invoke_enter";
const INVOKE_EXIT: &str = "__zkjs_invoke_exit";
const INVOKE_WRITE: &str = "__zkjs_invoke_write";
const INVOKE_RESULT: &str = "__zkjs_invoke_result";

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

    // Pre-resolved invoker functions, keyed by invoker name. Populated once in `new` only for the
    // hooks the user tracer actually defines, so a missing hook is a cheap map miss (no-op) rather
    // than a per-step `typeof` eval. The hot path calls these instead of re-`eval`ing JS source.
    invokers: HashMap<&'static str, JsObject>,

    // Execution bound: start time for the wall-clock deadline (see `JS_TRACER_EXECUTION_DEADLINE`).
    started_at: Instant,

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

        // Bound runaway JS loops inside a single hook invocation. Boa's recursion/stack limits are
        // already finite by default; only the loop-iteration limit defaults to unbounded.
        ctx.runtime_limits_mut()
            .set_loop_iteration_limit(JS_TRACER_MAX_LOOP_ITERATIONS);

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

        let invokers = resolve_invokers(&mut ctx)?;

        Ok(Self {
            ctx,
            tracer_config,
            invokers,
            started_at: Instant::now(),
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

    /// Calls a pre-installed invoker function (e.g. `__zkjs_invoke_step`) with the per-hook data.
    ///
    /// `arg` is converted to a `JsValue` via `JsValue::from_json` (GC-heap allocation, no source
    /// parsing and no interner growth) and passed to the invoker, which builds the geth-shaped
    /// `log`/`frame` wrapper and calls the user's tracer method. If the invoker is absent (the user
    /// tracer doesn't define that hook) this is a no-op, matching the previous existence check.
    fn invoke_named(
        &mut self,
        invoker: &'static str,
        arg: &JsonValue,
        method: TracerMethod,
    ) -> anyhow::Result<()> {
        let Some(f) = self.invokers.get(invoker).cloned() else {
            return Ok(());
        };

        let js_arg = JsValue::from_json(arg, &mut self.ctx).map_err(|e| {
            anyhow::anyhow!(
                "JS tracer argument conversion for {} failed: {e}",
                method.as_str()
            )
        })?;
        f.call(&JsValue::undefined(), &[js_arg], &mut self.ctx)
            .map_err(|e| anyhow::anyhow!("JS tracer method {} failed: {e}", method.as_str()))?;

        Ok(())
    }

    /// Returns true once the tracer has been running longer than `JS_TRACER_EXECUTION_DEADLINE`.
    /// Checked between EVM opcode steps so a long/heavy trace aborts instead of hanging.
    fn execution_budget_exceeded(&self) -> bool {
        self.started_at.elapsed() >= JS_TRACER_EXECUTION_DEADLINE
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

    fn invoke_method(&mut self, method: TracerMethod, arg: &JsonValue) {
        if self.error.is_some() {
            return;
        }

        let result = match method {
            TracerMethod::Setup => self.invoke_named(INVOKE_SETUP, arg, method),
            TracerMethod::Write => self.invoke_named(INVOKE_WRITE, arg, method),
            TracerMethod::Enter => self.invoke_named(INVOKE_ENTER, arg, method),
            TracerMethod::Exit => self.invoke_named(INVOKE_EXIT, arg, method),
            TracerMethod::Step | TracerMethod::Fault => {
                // Geth exposes a richer `log` to `step`, but only `{getError, getDepth}` when an
                // error is present. Preserve that split by picking the matching invoker (which
                // builds the appropriate wrapper shape).
                let has_error = arg.get("error").map(|e| !e.is_null()).unwrap_or(false);
                let invoker = match (method, has_error) {
                    (TracerMethod::Step, false) => INVOKE_STEP,
                    (TracerMethod::Step, true) => INVOKE_STEP_ERR,
                    (TracerMethod::Fault, false) => INVOKE_FAULT,
                    (TracerMethod::Fault, true) => INVOKE_FAULT_ERR,
                    _ => unreachable!(),
                };
                self.invoke_named(invoker, arg, method)
            }
            TracerMethod::Result => Err(anyhow::anyhow!(
                "Result must be invoked via call_result, not invoke_method"
            )),
            TracerMethod::StorageRead => Err(anyhow::anyhow!(
                "Storage read is not supported by JS tracer"
            )),
        };

        if let Err(err) = result {
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

        let Some(f) = self.invokers.get(INVOKE_RESULT).cloned() else {
            return Err(anyhow::anyhow!("JS tracer must define a 'result' function"));
        };

        let method_name = TracerMethod::Result.as_str();
        let arg = JsValue::from_json(&ctx, &mut self.ctx)
            .map_err(|e| anyhow::anyhow!("JS tracer result ctx conversion failed: {e}"))?;
        // The invoker returns `JSON.stringify(tracer.result(ctx, db))`, i.e. a JS string, matching
        // the previous behaviour exactly.
        let value = f
            .call(&JsValue::undefined(), &[arg], &mut self.ctx)
            .map_err(|e| anyhow::anyhow!("JS tracer method {method_name} failed: {e}"))?;

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

/// Resolves the per-hook invoker functions installed by `host::install_invocation_helpers`, but
/// only for the hooks the user tracer actually defines. A missing entry means the hook is absent,
/// so the hot path simply skips it (no per-step `typeof` eval needed).
fn resolve_invokers(ctx: &mut BoaContext) -> anyhow::Result<HashMap<&'static str, JsObject>> {
    let mut invokers = HashMap::new();

    // (tracer method name, invoker functions that drive it)
    let hooks: &[(&str, &[&'static str])] = &[
        ("setup", &[INVOKE_SETUP]),
        ("step", &[INVOKE_STEP, INVOKE_STEP_ERR]),
        ("fault", &[INVOKE_FAULT, INVOKE_FAULT_ERR]),
        ("enter", &[INVOKE_ENTER]),
        ("exit", &[INVOKE_EXIT]),
        ("write", &[INVOKE_WRITE]),
        ("result", &[INVOKE_RESULT]),
    ];

    for (method, invoker_names) in hooks {
        if !tracer_has_method(ctx, method)? {
            continue;
        }
        for name in *invoker_names {
            invokers.insert(*name, resolve_callable(ctx, name)?);
        }
    }

    Ok(invokers)
}

/// Checks once (at construction) whether the user tracer defines a callable hook of the given name.
fn tracer_has_method(ctx: &mut BoaContext, method: &str) -> anyhow::Result<bool> {
    let snippet = format!(
        "(typeof tracer === 'object' && tracer !== null && typeof tracer.{method} === 'function')"
    );
    let value = ctx
        .eval(Source::from_bytes(snippet.as_bytes()))
        .map_err(|e| anyhow::anyhow!(format!("JS tracer method existence check failed: {e:?}")))?;
    Ok(value.to_boolean())
}

/// Resolves a global function (installed in the Boa context) into a reusable callable handle.
/// The handle stays valid for the life of the tracer because the function is reachable from the
/// global object (a GC root), so it is never collected.
fn resolve_callable(ctx: &mut BoaContext, name: &str) -> anyhow::Result<JsObject> {
    let global = ctx.global_object();
    let value = global
        .get(js_string!(name), ctx)
        .map_err(|e| anyhow::anyhow!(format!("failed to resolve {name}: {e:?}")))?;
    value
        .as_callable()
        .ok_or_else(|| anyhow::anyhow!(format!("{name} is not callable")))
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
        // Once an error is recorded the tracer is done; skip all per-step work (including the
        // expensive heap/stack snapshot in `prepare_log_input`) so the EVM can unwind cheaply.
        if self.error.is_some() {
            return;
        }
        if self.execution_budget_exceeded() {
            self.record_error(
                TracerMethod::Step,
                anyhow::anyhow!(
                    "JS tracer exceeded execution time limit of {}s",
                    JS_TRACER_EXECUTION_DEADLINE.as_secs()
                ),
            );
            return;
        }
        // Nothing to do (and no point snapshotting memory/stack) if the tracer has no `step` hook.
        if !self.invokers.contains_key(INVOKE_STEP) {
            self.pending_step = None;
            return;
        }

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
        if self.error.is_some() {
            return;
        }
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
    block_context: zksync_os_storage_api::BlockContext,
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
