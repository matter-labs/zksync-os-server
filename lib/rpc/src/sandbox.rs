use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::trace::geth::{CallConfig, CallFrame, CallLogFrame};
use alloy::sol_types::{ContractError, GenericRevertReason};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmTracer,
    NopTracer, NopValidator,
};
use zksync_os_interface::traits::{NoopTxCallback, TxListSource};
use zksync_os_interface::types::{BlockContext, ExecutionResult, TxOutput};
use zksync_os_multivm::{run_block, simulate_tx};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

/// EVM max stack size.
pub const STACK_SIZE: usize = 1024;
/// zksync-os ergs per gas.
pub const ERGS_PER_GAS: u64 = 256;

/// Error message used when the VM terminates a transaction due to resource exhaustion
/// (out of native resources or pubdata limit exceeded).
pub(crate) const RESOURCE_EXHAUSTION_ERROR: &str =
    "ZKsync OS: out of execution resources or pubdata";
/// This message is used only for transactions whose top-level EVM execution succeeded,
/// but whose final tx result was flipped to revert by a post-execution bootloader check.
/// In the current bootloader implementation, that path is specific to pubdata charging.
/// If additional post-execution success-to-revert paths are introduced, this message and
/// the reconciliation logic must be revisited.
pub(crate) const POST_EXECUTION_PUBDATA_ERROR: &str =
    "execution reverted: insufficient gas to cover pubdata cost";

pub fn execute(
    tx: ZkTransaction,
    block_context: BlockContext,
    state_view: impl ViewState,
) -> anyhow::Result<Result<TxOutput, InvalidTransaction>> {
    let encoded_tx = tx.encode();

    simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut NopTracer,
    )
}

pub fn call_trace_simulate(
    tx: ZkTransaction,
    mut block_context: BlockContext,
    state_view: impl ViewState,
    call_config: CallConfig,
) -> anyhow::Result<CallFrame> {
    let mut tracer = CallTracer::new_with_config(
        vec![tx.clone()],
        call_config.with_log.unwrap_or_default(),
        call_config.only_top_call.unwrap_or_default(),
    );
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    let tx_result = simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut tracer,
    )?;

    let frame = tracer
        .transactions
        .last_mut()
        .expect("no transaction traced");
    let top_level_execution_succeeded = tracer
        .top_level_execution_succeeded
        .last()
        .copied()
        .unwrap_or(false);
    if let Ok(tx_output) = tx_result {
        reconcile_trace_with_output(frame, &tx_output, top_level_execution_succeeded);
    }

    Ok(std::mem::take(frame))
}

pub fn call_trace(
    txs: Vec<ZkTransaction>,
    block_context: BlockContext,
    state_view: impl ViewState,
    call_config: CallConfig,
) -> anyhow::Result<Vec<CallFrame>> {
    let mut tracer = CallTracer::new_with_config(
        txs.clone(),
        call_config.with_log.unwrap_or_default(),
        call_config.only_top_call.unwrap_or_default(),
    );

    let tx_source = TxListSource {
        transactions: txs.into_iter().map(|tx| tx.encode()).collect(),
    };
    let block_output = run_block(
        block_context,
        state_view.clone(),
        state_view,
        tx_source,
        NoopTxCallback,
        &mut tracer,
        &mut NopValidator,
    )?;

    anyhow::ensure!(
        tracer.transactions.len() == block_output.tx_results.len(),
        "tracer recorded {} frames but VM returned {} results",
        tracer.transactions.len(),
        block_output.tx_results.len(),
    );
    anyhow::ensure!(
        tracer.transactions.len() == tracer.top_level_execution_succeeded.len(),
        "tracer recorded {} frames but tracked {} top-level execution results",
        tracer.transactions.len(),
        tracer.top_level_execution_succeeded.len(),
    );
    for ((frame, tx_result), top_level_execution_succeeded) in tracer
        .transactions
        .iter_mut()
        .zip(block_output.tx_results.iter())
        .zip(tracer.top_level_execution_succeeded.iter().copied())
    {
        if let Ok(tx_output) = tx_result {
            reconcile_trace_with_output(frame, tx_output, top_level_execution_succeeded);
        }
    }

    Ok(tracer.transactions)
}

/// Reconciles the tracer's view of a transaction with the actual execution result.
///
/// The tracer sees the EVM execution step but misses post-execution checks done by the
/// bootloader (e.g. pubdata cost verification). This function patches the trace only
/// when the top-level frame itself succeeded, but the final tx result is revert.
///
/// The error message below intentionally relies on the current bootloader invariant:
/// the only known post-execution success-to-revert transition is the pubdata charge
/// check. If that stops being true, this logic needs to be generalized.
fn reconcile_trace_with_output(
    frame: &mut CallFrame,
    tx_output: &TxOutput,
    top_level_execution_succeeded: bool,
) {
    if top_level_execution_succeeded
        && let ExecutionResult::Revert(revert_bytes) = &tx_output.execution_result
    {
        frame.gas_used = U256::from(tx_output.gas_used);
        frame.error = Some(POST_EXECUTION_PUBDATA_ERROR.to_string());
        frame.output = Some(Bytes::copy_from_slice(revert_bytes));
        frame.revert_reason = None;
        if frame.typ == "CREATE" || frame.typ == "CREATE2" {
            frame.to = None;
        }
    }
}

#[derive(Default)]
pub struct CallTracer {
    input_transactions: Vec<ZkTransaction>,
    transactions: Vec<CallFrame>,
    top_level_execution_succeeded: Vec<bool>,
    unfinished_calls: Vec<CallFrame>,
    finished_calls: Vec<CallFrame>,
    current_call_depth: usize,
    current_tx_top_level_execution_succeeded: bool,
    collect_logs: bool,
    only_top_call: bool,

    create_operation_requested: Option<CreateType>,
}

#[derive(Debug)]
enum CreateType {
    Create,
    Create2,
}

impl CallTracer {
    pub fn new_with_config(
        input_transactions: Vec<ZkTransaction>,
        collect_logs: bool,
        only_top_call: bool,
    ) -> Self {
        Self {
            input_transactions,
            transactions: vec![],
            top_level_execution_succeeded: vec![],
            unfinished_calls: vec![],
            finished_calls: vec![],
            current_call_depth: 0,
            current_tx_top_level_execution_succeeded: false,
            collect_logs,
            only_top_call,
            create_operation_requested: None,
        }
    }
}

impl AnyTracer for CallTracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl EvmTracer for CallTracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        self.current_call_depth += 1;

        if !self.only_top_call || self.current_call_depth == 1 {
            // Top-level deployment (initiated by EOA) won't trigger `on_create_request` hook
            // This is always a CREATE
            if self.current_call_depth == 1 && request.modifier() == CallModifier::Constructor {
                self.create_operation_requested = Some(CreateType::Create);
            }

            self.unfinished_calls.push(CallFrame {
                from: request.caller(),
                gas: U256::from(request.resources().ergs / ERGS_PER_GAS),
                gas_used: U256::ZERO, // will be populated later
                to: Some(request.callee()),
                input: Bytes::copy_from_slice(request.input()),
                output: None,        // will be populated later
                error: None,         // can be populated later
                revert_reason: None, // can be populated later
                calls: vec![],       // will be populated later
                logs: vec![],        // will be populated later
                value: if request.modifier() == CallModifier::Static {
                    // STATICCALL frames don't have `value`
                    None
                } else {
                    Some(request.nominal_token_value())
                },
                typ: match request.modifier() {
                    CallModifier::NoModifier => "CALL",
                    CallModifier::Constructor => {
                        match self
                            .create_operation_requested
                            .as_ref()
                            .expect("Should exist")
                        {
                            CreateType::Create => "CREATE",
                            CreateType::Create2 => "CREATE2",
                        }
                    }
                    CallModifier::Delegate | CallModifier::DelegateStatic => "DELEGATECALL",
                    CallModifier::Static => "STATICCALL",
                    CallModifier::EVMCallcode | CallModifier::EVMCallcodeStatic => "CALLCODE",
                    // Call types below are unused and are not expected to be present in the trace
                    CallModifier::ZKVMSystem => {
                        panic!("unexpected call type: ZKVMSystem")
                    }
                    CallModifier::ZKVMSystemStatic => {
                        panic!("unexpected call type: ZKVMSystemStatic")
                    }
                }
                .to_string(),
            })
        }

        // Reset flag, required data is consumed
        if self.create_operation_requested.is_some() {
            self.create_operation_requested = None;
        }
    }

    fn after_execution_frame_completed(&mut self, result: Option<(EvmResources, CallResult)>) {
        assert_ne!(self.current_call_depth, 0);
        let is_top_level_frame = self.current_call_depth == 1;
        let top_level_execution_succeeded =
            matches!(&result, Some((_, CallResult::Successful { .. })));

        if !self.only_top_call || self.current_call_depth == 1 {
            let mut finished_call = self.unfinished_calls.pop().expect("Should exist");

            match result {
                Some((resources, result)) => {
                    finished_call.gas_used = finished_call
                        .gas
                        .saturating_sub(U256::from(resources.ergs / ERGS_PER_GAS));

                    match result {
                        CallResult::Failed { returndata } => {
                            finished_call.revert_reason = maybe_revert_reason(returndata);
                            finished_call.output = Some(Bytes::copy_from_slice(returndata));
                            if finished_call.typ == "CREATE" || finished_call.typ == "CREATE2" {
                                // Clear `to` field as no contract was created
                                finished_call.to = None;
                            }
                        }
                        CallResult::Successful { returndata } => {
                            if finished_call.typ == "CREATE" || finished_call.typ == "CREATE2" {
                                // output should be already populated in `on_bytecode_change` hook
                            } else {
                                finished_call.output = Some(Bytes::copy_from_slice(returndata));
                            }
                        }
                    };
                }
                None => {
                    // Some unexpected internal failure happened (maybe out of native resources)
                    // Should revert whole tx
                    finished_call.gas_used = finished_call.gas;
                    finished_call.output = None;
                    finished_call.revert_reason = None;
                    if finished_call.typ == "CREATE" || finished_call.typ == "CREATE2" {
                        // Clear `to` field as no contract was created
                        finished_call.to = None;
                    }

                    if self.current_call_depth == 1 {
                        // Add error info to the top-level call

                        // Note: we can't distinguish runtime resources exhaustion from fatal internal errors here.
                        // Tracer should not be used if VM panics.
                        finished_call.error = Some(RESOURCE_EXHAUSTION_ERROR.to_string());
                    }
                }
            }

            if is_top_level_frame {
                self.current_tx_top_level_execution_succeeded = top_level_execution_succeeded;
            }
            if let Some(parent_call) = self.unfinished_calls.last_mut() {
                parent_call.calls.push(finished_call);
            } else {
                self.finished_calls.push(finished_call);
            }
        }

        self.current_call_depth -= 1;

        // Reset flag in case if frame terminated due to out-of-native / other internal ZKsync OS error
        if self.create_operation_requested.is_some() {
            self.create_operation_requested = None;
        }
    }

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_call_depth = 0;
        self.current_tx_top_level_execution_succeeded = false;

        // Sanity check
        assert!(self.create_operation_requested.is_none());
    }

    fn finish_tx(&mut self) {
        assert_eq!(self.current_call_depth, 0);
        assert!(self.unfinished_calls.is_empty());

        // Sanity check
        assert!(self.create_operation_requested.is_none());

        if let Some(top_level_call) = self.finished_calls.pop() {
            self.transactions.push(top_level_call);
            self.top_level_execution_succeeded
                .push(self.current_tx_top_level_execution_succeeded);
        } else {
            // We can have some edge cases when tx fails before any call frame is created
            // In this case currently we populate minimal call frame info from the input tx data
            let empty_tx = self.input_transactions.get(self.transactions.len());
            if let Some(tx) = empty_tx {
                self.transactions.push(CallFrame {
                    from: tx.signer(),
                    gas: U256::from(tx.gas_limit()),
                    gas_used: U256::from(tx.gas_limit()),
                    to: tx.to(),
                    input: tx.input().clone(),
                    output: None,
                    error: Some("transaction failed before execution".to_string()),
                    revert_reason: None,
                    calls: vec![],
                    logs: vec![],
                    value: Some(tx.value()), // Can't have STATICCALL here
                    typ: if tx.to().is_some() {
                        "CALL".to_string()
                    } else {
                        "CREATE".to_string()
                    },
                });
                self.top_level_execution_succeeded.push(false);
            }
        }
    }

    fn on_event(&mut self, address: Address, topics: Vec<B256>, data: &[u8]) {
        if self.collect_logs {
            let call = self.unfinished_calls.last_mut().expect("Should exist");
            call.logs.push(CallLogFrame {
                address: if address == Address::ZERO {
                    None
                } else {
                    Some(address)
                },
                topics: if topics.is_empty() {
                    None
                } else {
                    Some(topics)
                },
                data: if data.is_empty() {
                    None
                } else {
                    Some(Bytes::copy_from_slice(data))
                },
                // todo: populate
                position: None,
                index: None,
            })
        }
    }

    fn on_storage_read(
        &mut self,
        _is_transient: bool,
        _address: Address,
        _key: B256,
        _value: B256,
    ) {
    }

    fn on_storage_write(
        &mut self,
        _is_transient: bool,
        _address: Address,
        _key: B256,
        _value: B256,
    ) {
    }

    fn on_bytecode_change(
        &mut self,
        address: Address,
        new_raw_bytecode: Option<&[u8]>,
        _new_internal_bytecode_hash: B256,
        new_observable_bytecode_length: u32,
    ) {
        let call = self.unfinished_calls.last_mut().expect("Should exist");

        if call.typ == "CREATE" || call.typ == "CREATE2" {
            assert_eq!(address, call.to.expect("Should exist"));
            let deployed_raw_bytecode = new_raw_bytecode.expect("Should be present");

            assert!(deployed_raw_bytecode.len() >= new_observable_bytecode_length as usize);

            // raw bytecode may include internal artifacts (jumptable), so we need to trim it
            call.output = Some(Bytes::copy_from_slice(
                &deployed_raw_bytecode[..new_observable_bytecode_length as usize],
            ));
        } else {
            // should not happen now (system hooks currently do not trigger this hook)
        }
    }

    #[inline(always)]
    fn before_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        _frame_state: impl EvmFrameInterface,
    ) {
    }

    #[inline(always)]
    fn after_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        _frame_state: impl EvmFrameInterface,
    ) {
    }

    /// Opcode failed for some reason. Note: call frame ends immediately
    fn on_opcode_error(&mut self, error: &EvmError, _frame_state: impl EvmFrameInterface) {
        if self.only_top_call && self.current_call_depth > 1 {
            // Ignore errors in subcalls if only the top call should be traced
            return;
        }

        let current_call = self.unfinished_calls.last_mut().expect("Should exist");
        current_call.error = Some(fmt_error_msg(error));

        // In case we fail after `on_create_request` hook, but before `on_new_execution_frame` hook
        if self.create_operation_requested.is_some() {
            self.create_operation_requested = None;
        }
    }

    /// Special cases, when error happens in frame before any opcode is executed (unfortunately we can't provide access to state)
    /// Note: call frame ends immediately
    fn on_call_error(&mut self, error: &EvmError) {
        if self.only_top_call && self.current_call_depth > 1 {
            // Ignore errors in subcalls if only the top call should be traced
            return;
        }

        let current_call = self.unfinished_calls.last_mut().expect("Should exist");
        current_call.error = Some(fmt_error_msg(error));

        // Sanity check
        assert!(self.create_operation_requested.is_none());
    }

    /// We should treat selfdestruct as a special kind of a call
    fn on_selfdestruct(
        &mut self,
        beneficiary: Address,
        token_value: U256,
        frame_state: impl EvmFrameInterface,
    ) {
        // Following Geth implementation: https://github.com/ethereum/go-ethereum/blob/2dbb580f51b61d7ff78fceb44b06835827704110/core/vm/instructions.go#L894
        //
        // It's debatable whether post-Cancun SELFDESTRUCT invocation should create a "SELFDESTURCT"
        // frame for "old" contracts that cannot be destroyed.
        // * reth treats such calls as "CALL" frames
        // * geth treats such calls as "SELFDESTRUCT" frames, but there is an issue that debates
        //   this behavior (https://github.com/ethereum/go-ethereum/issues/32376)
        let call_frame = CallFrame {
            from: frame_state.address(),
            gas: Default::default(),
            gas_used: Default::default(),
            to: Some(beneficiary),
            input: Default::default(),
            output: None,
            error: None,
            revert_reason: None,
            calls: vec![],
            logs: vec![],
            value: Some(token_value),
            typ: "SELFDESTRUCT".to_string(),
        };

        if let Some(parent_call) = self.unfinished_calls.last_mut() {
            parent_call.calls.push(call_frame);
        } else {
            self.finished_calls.push(call_frame);
        }
    }

    fn on_create_request(&mut self, is_create2: bool) {
        // Can't be some - `on_new_execution_frame` or `on_opcode_error` should reset flag
        assert!(self.create_operation_requested.is_none());

        self.create_operation_requested = if is_create2 {
            Some(CreateType::Create)
        } else {
            Some(CreateType::Create2)
        };
    }
}

/// Returns a non-empty revert reason if the output is a revert/error.
pub(crate) fn maybe_revert_reason(output: &[u8]) -> Option<String> {
    let reason = match GenericRevertReason::decode(output)? {
        GenericRevertReason::ContractError(err) => {
            match err {
                // return the raw revert reason and don't use the revert's display message
                ContractError::Revert(revert) => revert.reason,
                err => err.to_string(),
            }
        }
        GenericRevertReason::RawString(err) => err,
    };
    if reason.is_empty() {
        None
    } else {
        Some(reason)
    }
}

/// Converts [`EvmError`] to a geth-style error message (if possible).
///
/// See https://github.com/ethereum/go-ethereum/blob/9ce40d19a8240844be24b9692c639dff45d13d68/core/vm/errors.go#L26-L45
pub(crate) fn fmt_error_msg(error: &EvmError) -> String {
    match error {
        // todo: missing `ErrGasUintOverflow`: likely not propagated during tx decoding
        EvmError::Revert => "execution reverted".to_string(),
        EvmError::OutOfGas => "out of gas".to_string(),
        EvmError::InvalidJump => "invalid jump destination".to_string(),
        EvmError::ReturnDataOutOfBounds => "return data out of bounds".to_string(),
        EvmError::InvalidOpcode(opcode) => format!("invalid opcode: {opcode}"),
        EvmError::StackUnderflow => "stack underflow".to_string(),
        EvmError::StackOverflow => {
            format!("stack limit reached {} ({})", STACK_SIZE, STACK_SIZE - 1)
        }
        EvmError::CallNotAllowedInsideStatic => "write protection".to_string(),
        EvmError::StateChangeDuringStaticCall => "write protection".to_string(),
        // geth returns "out of gas", we provide a more fine-grained error
        EvmError::MemoryLimitOOG => format!("out of gas (memory limit reached {}))", u32::MAX - 31),
        // geth returns "out of gas", we provide a more fine-grained error
        EvmError::InvalidOperandOOG => "out of gas (invalid operand)".to_string(),
        EvmError::CodeStoreOutOfGas => "contract creation code storage out of gas".to_string(),
        EvmError::CallTooDeep => "max call depth exceeded".to_string(),
        EvmError::InsufficientBalance => "insufficient balance for transfer".to_string(),
        EvmError::CreateCollision => "contract address collision".to_string(),
        EvmError::NonceOverflow => "nonce uint64 overflow".to_string(),
        EvmError::CreateContractSizeLimit => "max code size exceeded".to_string(),
        EvmError::CreateInitcodeSizeLimit => "max initcode size exceeded".to_string(),
        EvmError::CreateContractStartingWithEF => {
            "invalid code: must not begin with 0xef".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::{Revert, SolError};
    use zksync_os_interface::types::ExecutionOutput;

    fn make_tx_output(execution_result: ExecutionResult) -> TxOutput {
        TxOutput {
            execution_result,
            gas_used: 50_000,
            gas_refunded: 0,
            computational_native_used: 100,
            native_used: 200,
            pubdata_used: 300,
            contract_address: None,
            logs: vec![],
            l2_to_l1_logs: vec![],
            storage_writes: vec![],
        }
    }

    fn make_empty_call_frame() -> CallFrame {
        CallFrame {
            from: Address::ZERO,
            gas: U256::ZERO,
            gas_used: U256::ZERO,
            to: None,
            input: Bytes::new(),
            output: Some(Bytes::from(vec![0xaa, 0xbb])),
            error: None,
            revert_reason: None,
            calls: vec![],
            logs: vec![],
            value: None,
            typ: "CALL".to_string(),
        }
    }

    fn make_create_call_frame() -> CallFrame {
        CallFrame {
            to: Some(Address::from([0x11; 20])),
            typ: "CREATE".to_string(),
            output: Some(Bytes::from(vec![0xca, 0xfe])),
            ..make_empty_call_frame()
        }
    }

    #[test]
    fn reconcile_patches_frame_when_tracer_missed_revert() {
        let tx_output = make_tx_output(ExecutionResult::Revert(
            Revert::from("coincidental success bytes").abi_encode(),
        ));
        let mut frame = make_empty_call_frame();

        reconcile_trace_with_output(&mut frame, &tx_output, true);

        let error = frame.error.expect("missing patched error");
        assert_eq!(error, POST_EXECUTION_PUBDATA_ERROR);
        assert_eq!(frame.gas_used, U256::from(50_000));
        assert_eq!(
            frame.output,
            Some(Bytes::from(
                Revert::from("coincidental success bytes").abi_encode()
            ))
        );
        assert!(frame.revert_reason.is_none());
    }

    #[test]
    fn reconcile_no_op_when_top_level_execution_failed() {
        let tx_output = make_tx_output(ExecutionResult::Revert(vec![]));
        let mut frame = make_empty_call_frame();
        frame.error = Some("execution reverted".to_string());
        frame.output = Some(Bytes::new());

        reconcile_trace_with_output(&mut frame, &tx_output, false);

        let error = frame.error.expect("missing preserved error");
        assert_eq!(error, "execution reverted");
        assert_eq!(frame.gas_used, U256::ZERO);
        assert_eq!(frame.output, Some(Bytes::new()));
        assert!(frame.revert_reason.is_none());
    }

    #[test]
    fn reconcile_no_op_on_success() {
        let tx_output = make_tx_output(ExecutionResult::Success(ExecutionOutput::Call(vec![
            1, 2, 3,
        ])));
        let mut frame = make_empty_call_frame();
        let original_output = frame.output.clone();

        reconcile_trace_with_output(&mut frame, &tx_output, true);

        assert!(frame.error.is_none());
        assert_eq!(frame.output, original_output);
        assert_eq!(frame.gas_used, U256::ZERO);
    }

    #[test]
    fn reconcile_keeps_existing_non_resource_error() {
        let tx_output = make_tx_output(ExecutionResult::Revert(vec![0xde, 0xad]));
        let mut frame = make_empty_call_frame();
        frame.error = Some("execution reverted".to_string());

        reconcile_trace_with_output(&mut frame, &tx_output, false);

        assert_eq!(frame.error.as_deref(), Some("execution reverted"));
        assert_eq!(frame.output, Some(Bytes::from(vec![0xaa, 0xbb])));
        assert_eq!(frame.gas_used, U256::ZERO);
    }

    #[test]
    fn reconcile_clears_created_address_for_post_execution_revert() {
        let tx_output = make_tx_output(ExecutionResult::Revert(vec![]));
        let mut frame = make_create_call_frame();

        reconcile_trace_with_output(&mut frame, &tx_output, true);

        assert_eq!(frame.error.as_deref(), Some(POST_EXECUTION_PUBDATA_ERROR));
        assert!(frame.to.is_none());
    }
}
