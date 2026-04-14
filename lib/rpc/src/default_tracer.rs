use crate::sandbox::{ERGS_PER_GAS, fmt_error_msg};
use alloy::hex;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::trace::geth::{DefaultFrame, GethDefaultTracingOptions, StructLog};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmStackInterface,
    EvmTracer, NopValidator,
};
use zksync_os_interface::traits::{NoopTxCallback, TxListSource};
use zksync_os_interface::types::{BlockContext, ExecutionOutput, ExecutionResult, TxOutput};
use zksync_os_multivm::{run_block, simulate_tx};
use zksync_os_storage_api::ViewState;
use zksync_os_types::{ZkTransaction, ZksyncOsEncode};

pub(crate) fn default_trace_simulate(
    tx: ZkTransaction,
    mut block_context: BlockContext,
    state_view: impl ViewState,
    opts: GethDefaultTracingOptions,
) -> anyhow::Result<DefaultFrame> {
    let mut tracer = DefaultTracer::new(opts);
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    let tx_result = simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut tracer,
    )?;

    let mut frame = tracer.results.pop().unwrap_or_default();
    if let Ok(tx_output) = tx_result {
        reconcile_default_trace_with_output(&mut frame, &tx_output);
    }

    Ok(frame)
}

pub(crate) fn default_trace(
    txs: Vec<ZkTransaction>,
    block_context: BlockContext,
    state_view: impl ViewState,
    opts: GethDefaultTracingOptions,
) -> anyhow::Result<Vec<DefaultFrame>> {
    let mut tracer = DefaultTracer::new(opts);

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
        tracer.results.len() == block_output.tx_results.len(),
        "tracer recorded {} frames but VM returned {} results",
        tracer.results.len(),
        block_output.tx_results.len(),
    );

    for (frame, tx_result) in tracer
        .results
        .iter_mut()
        .zip(block_output.tx_results.iter())
    {
        if let Ok(tx_output) = tx_result {
            reconcile_default_trace_with_output(frame, tx_output);
        }
    }

    Ok(tracer.results)
}

fn reconcile_default_trace_with_output(frame: &mut DefaultFrame, tx_output: &TxOutput) {
    frame.gas = tx_output.gas_used;
    match &tx_output.execution_result {
        ExecutionResult::Success(
            ExecutionOutput::Call(return_data) | ExecutionOutput::Create(return_data, _),
        ) => {
            frame.failed = false;
            frame.return_value = Bytes::copy_from_slice(return_data);
        }
        ExecutionResult::Revert(return_data) => {
            frame.failed = true;
            frame.return_value = Bytes::copy_from_slice(return_data);
        }
    }
}

#[derive(Debug)]
struct DefaultTracer {
    opts: GethDefaultTracingOptions,
    results: Vec<DefaultFrame>,
    current_struct_logs: Vec<StructLog>,
    current_depth: usize,
    current_tx_failed: bool,
    current_tx_gas: u64,
    current_tx_return_value: Bytes,
    steps_counter: usize,
    storage_caches_for_frames: Vec<BTreeMap<B256, B256>>,
    last_known_gas_left: u64,
    gas_used_by_last_call: u64,
    gas_used_by_calls: Vec<u64>,
    pending_call_opcodes: HashMap<usize, (usize, u64)>,
}

impl DefaultTracer {
    fn new(opts: GethDefaultTracingOptions) -> Self {
        Self {
            opts,
            results: Vec::new(),
            current_struct_logs: Vec::new(),
            current_depth: 0,
            current_tx_failed: false,
            current_tx_gas: 0,
            current_tx_return_value: Bytes::new(),
            steps_counter: 0,
            storage_caches_for_frames: Vec::new(),
            last_known_gas_left: 0,
            gas_used_by_last_call: 0,
            gas_used_by_calls: Vec::new(),
            pending_call_opcodes: HashMap::new(),
        }
    }

    fn log_limit(&self) -> usize {
        self.opts.limit.unwrap_or(0) as usize
    }

    fn is_step_recording_limited(&self) -> bool {
        let limit = self.log_limit();
        limit != 0 && self.steps_counter >= limit
    }

    fn format_memory(memory: &[u8]) -> Vec<String> {
        memory
            .chunks(32)
            .map(|chunk| {
                let mut padded = [0_u8; 32];
                padded[..chunk.len()].copy_from_slice(chunk);
                hex::encode(padded)
            })
            .collect()
    }

    fn patch_pending_call_opcode(&mut self, gas_left_after_call: u64) {
        let Some((opcode_log_index, gas_before_call)) =
            self.pending_call_opcodes.remove(&self.current_depth)
        else {
            return;
        };

        let Some(log) = self.current_struct_logs.get_mut(opcode_log_index) else {
            return;
        };
        log.gas_cost = gas_before_call
            .saturating_sub(gas_left_after_call)
            .saturating_sub(self.gas_used_by_last_call);
    }

    fn current_storage_snapshot(&self) -> Option<BTreeMap<B256, B256>> {
        self.storage_caches_for_frames.last().cloned()
    }

    fn update_last_log(&mut self, frame_state: &impl EvmFrameInterface, error: Option<String>) {
        let gas_after = frame_state.resources().ergs / ERGS_PER_GAS;
        let return_data = self
            .opts
            .is_return_data_enabled()
            .then(|| Bytes::copy_from_slice(frame_state.return_data()));
        let storage = self.current_storage_snapshot();

        let Some(log) = self.current_struct_logs.last_mut() else {
            return;
        };

        log.gas_cost = self.last_known_gas_left.saturating_sub(gas_after);
        log.error = error;
        if self.opts.is_return_data_enabled() {
            log.return_data = return_data;
        }
        if self.opts.is_storage_enabled() && matches!(log.op.as_ref(), "SLOAD" | "SSTORE") {
            log.storage = Some(storage.unwrap_or_default());
        }
    }
}

impl AnyTracer for DefaultTracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl EvmTracer for DefaultTracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        if !self.current_struct_logs.is_empty() {
            self.pending_call_opcodes.insert(
                self.current_depth,
                (self.current_struct_logs.len() - 1, self.last_known_gas_left),
            );
        }

        self.current_depth += 1;

        if self.opts.is_storage_enabled() {
            self.storage_caches_for_frames.push(BTreeMap::new());
        }

        self.gas_used_by_calls
            .push(request.resources().ergs / ERGS_PER_GAS);
    }

    fn after_execution_frame_completed(&mut self, result: Option<(EvmResources, CallResult)>) {
        if let Some((resources, res)) = result {
            let gas_available_before_call = self
                .gas_used_by_calls
                .pop()
                .unwrap_or(resources.ergs / ERGS_PER_GAS);
            self.gas_used_by_last_call =
                gas_available_before_call.saturating_sub(resources.ergs / ERGS_PER_GAS);

            if self.current_depth == 1 {
                self.current_tx_gas = resources.ergs / ERGS_PER_GAS;
                match res {
                    CallResult::Successful { returndata } => {
                        self.current_tx_failed = false;
                        self.current_tx_return_value = Bytes::copy_from_slice(returndata);
                    }
                    CallResult::Failed { returndata } => {
                        self.current_tx_failed = true;
                        self.current_tx_return_value = Bytes::copy_from_slice(returndata);
                    }
                }
            }
        } else {
            let _ = self.gas_used_by_calls.pop();
            self.gas_used_by_last_call = 0;
            if self.current_depth == 1 {
                self.current_tx_failed = true;
                self.current_tx_gas = 0;
                self.current_tx_return_value = Bytes::new();
            }
        }

        self.current_depth = self.current_depth.saturating_sub(1);

        if self.opts.is_storage_enabled() {
            let _ = self.storage_caches_for_frames.pop();
        }
    }

    fn on_storage_read(&mut self, is_transient: bool, _address: Address, key: B256, value: B256) {
        if is_transient || !self.opts.is_storage_enabled() {
            return;
        }
        if let Some(storage) = self.storage_caches_for_frames.last_mut() {
            storage.insert(key, value);
        }
    }

    fn on_storage_write(&mut self, is_transient: bool, _address: Address, key: B256, value: B256) {
        if is_transient || !self.opts.is_storage_enabled() {
            return;
        }
        if let Some(storage) = self.storage_caches_for_frames.last_mut() {
            storage.insert(key, value);
        }
    }

    fn on_bytecode_change(
        &mut self,
        _address: Address,
        _new_raw_bytecode: Option<&[u8]>,
        _new_internal_bytecode_hash: B256,
        _new_observable_bytecode_length: u32,
    ) {
    }

    fn on_event(&mut self, _address: Address, _topics: Vec<B256>, _data: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_struct_logs.clear();
        self.current_depth = 0;
        self.current_tx_failed = false;
        self.current_tx_gas = 0;
        self.current_tx_return_value = Bytes::new();
        self.steps_counter = 0;
        self.storage_caches_for_frames.clear();
        self.last_known_gas_left = 0;
        self.gas_used_by_last_call = 0;
        self.gas_used_by_calls.clear();
        self.pending_call_opcodes.clear();
    }

    fn finish_tx(&mut self) {
        self.results.push(DefaultFrame {
            failed: self.current_tx_failed,
            gas: self.current_tx_gas,
            return_value: std::mem::take(&mut self.current_tx_return_value),
            struct_logs: std::mem::take(&mut self.current_struct_logs),
        });
    }

    fn before_evm_interpreter_execution_step(
        &mut self,
        opcode: u8,
        frame_state: impl EvmFrameInterface,
    ) {
        self.patch_pending_call_opcode(frame_state.resources().ergs / ERGS_PER_GAS);

        if self.is_step_recording_limited() {
            return;
        }
        self.steps_counter += 1;

        self.last_known_gas_left = frame_state.resources().ergs / ERGS_PER_GAS;

        let op_name = zk_os_evm_interpreter::opcodes::OPCODE_JUMPMAP[opcode as usize]
            .unwrap_or("Invalid opcode");
        let stack = self
            .opts
            .is_stack_enabled()
            .then(|| frame_state.stack().to_slice().to_vec());
        let memory = self
            .opts
            .is_memory_enabled()
            .then(|| Self::format_memory(frame_state.heap()));

        self.current_struct_logs.push(StructLog {
            pc: frame_state.instruction_pointer() as u64,
            op: Cow::Borrowed(op_name),
            gas: self.last_known_gas_left,
            gas_cost: 0,
            depth: self.current_depth as u64,
            error: None,
            stack,
            return_data: None,
            memory,
            memory_size: None,
            storage: None,
            refund_counter: Some(frame_state.refund_counter() as u64),
        });
    }

    fn after_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        frame_state: impl EvmFrameInterface,
    ) {
        self.update_last_log(&frame_state, None);
    }

    fn on_opcode_error(&mut self, error: &EvmError, frame_state: impl EvmFrameInterface) {
        self.update_last_log(&frame_state, Some(fmt_error_msg(error)));
    }

    fn on_call_error(&mut self, error: &EvmError) {
        let Some(log) = self.current_struct_logs.last_mut() else {
            return;
        };
        log.error = Some(fmt_error_msg(error));
    }

    fn on_selfdestruct(
        &mut self,
        _beneficiary: Address,
        _token_value: U256,
        _frame_state: impl EvmFrameInterface,
    ) {
    }

    fn on_create_request(&mut self, _is_create2: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_interface::tracing::EvmStackInterface;

    #[derive(Default)]
    struct TestStack(Vec<U256>);

    impl EvmStackInterface for TestStack {
        fn to_slice(&self) -> &[U256] {
            &self.0
        }

        fn len(&self) -> usize {
            self.0.len()
        }

        fn peek_n(&self, index: usize) -> Result<&U256, EvmError> {
            self.0
                .len()
                .checked_sub(index + 1)
                .and_then(|offset| self.0.get(offset))
                .ok_or(EvmError::StackUnderflow)
        }
    }

    #[derive(Default)]
    struct TestFrame {
        pc: usize,
        gas: u64,
        caller: Address,
        address: Address,
        calldata: Vec<u8>,
        return_data: Vec<u8>,
        heap: Vec<u8>,
        stack: TestStack,
        refund_counter: u32,
        call_value: U256,
        is_static: bool,
        is_constructor: bool,
    }

    impl EvmFrameInterface for TestFrame {
        fn instruction_pointer(&self) -> usize {
            self.pc
        }

        fn resources(&self) -> EvmResources {
            EvmResources {
                ergs: self.gas * ERGS_PER_GAS,
                native: 0,
            }
        }

        fn stack(&self) -> &impl EvmStackInterface {
            &self.stack
        }

        fn caller(&self) -> Address {
            self.caller
        }

        fn address(&self) -> Address {
            self.address
        }

        fn calldata(&self) -> &[u8] {
            &self.calldata
        }

        fn return_data(&self) -> &[u8] {
            &self.return_data
        }

        fn heap(&self) -> &[u8] {
            &self.heap
        }

        fn bytecode(&self) -> &[u8] {
            &[]
        }

        fn call_value(&self) -> &U256 {
            &self.call_value
        }

        fn refund_counter(&self) -> u32 {
            self.refund_counter
        }

        fn is_static(&self) -> bool {
            self.is_static
        }

        fn is_constructor(&self) -> bool {
            self.is_constructor
        }
    }

    #[derive(Default)]
    struct TestRequest {
        gas: u64,
        caller: Address,
        callee: Address,
        calldata: Vec<u8>,
        value: U256,
    }

    impl EvmRequest for TestRequest {
        fn resources(&self) -> EvmResources {
            EvmResources {
                ergs: self.gas * ERGS_PER_GAS,
                native: 0,
            }
        }

        fn caller(&self) -> Address {
            self.caller
        }

        fn callee(&self) -> Address {
            self.callee
        }

        fn modifier(&self) -> zksync_os_interface::tracing::CallModifier {
            zksync_os_interface::tracing::CallModifier::NoModifier
        }

        fn input(&self) -> &[u8] {
            &self.calldata
        }

        fn nominal_token_value(&self) -> U256 {
            self.value
        }
    }

    fn make_tx_output(execution_result: ExecutionResult) -> TxOutput {
        TxOutput {
            execution_result,
            gas_used: 42_000,
            gas_refunded: 0,
            computational_native_used: 0,
            native_used: 0,
            pubdata_used: 0,
            contract_address: None,
            logs: vec![],
            l2_to_l1_logs: vec![],
            storage_writes: vec![],
        }
    }

    #[test]
    fn default_tracer_respects_capture_flags_and_limit() {
        let mut tracer = DefaultTracer::new(GethDefaultTracingOptions {
            enable_memory: Some(true),
            disable_stack: Some(true),
            enable_return_data: Some(true),
            limit: Some(1),
            ..Default::default()
        });

        tracer.begin_tx(&[]);
        tracer.on_new_execution_frame(TestRequest {
            gas: 100,
            caller: Address::from([0x11; 20]),
            callee: Address::from([0x22; 20]),
            calldata: vec![],
            value: U256::ZERO,
        });

        tracer.before_evm_interpreter_execution_step(
            zk_os_evm_interpreter::opcodes::SLOAD,
            TestFrame {
                pc: 7,
                gas: 100,
                address: Address::from([0x22; 20]),
                heap: vec![0xaa; 40],
                return_data: vec![0xbb, 0xcc],
                stack: TestStack(vec![U256::from(1_u64), U256::from(2_u64)]),
                ..Default::default()
            },
        );
        tracer.on_storage_read(
            false,
            Address::from([0x22; 20]),
            B256::from([0x33; 32]),
            B256::from([0x44; 32]),
        );
        tracer.after_evm_interpreter_execution_step(
            zk_os_evm_interpreter::opcodes::SLOAD,
            TestFrame {
                pc: 7,
                gas: 97,
                address: Address::from([0x22; 20]),
                heap: vec![0xaa; 40],
                return_data: vec![0xbb, 0xcc],
                stack: TestStack(vec![U256::from(1_u64), U256::from(2_u64)]),
                ..Default::default()
            },
        );

        tracer.before_evm_interpreter_execution_step(
            zk_os_evm_interpreter::opcodes::STOP,
            TestFrame {
                pc: 8,
                gas: 97,
                ..Default::default()
            },
        );

        tracer.after_execution_frame_completed(Some((
            EvmResources {
                ergs: 55 * ERGS_PER_GAS,
                native: 0,
            },
            CallResult::Successful {
                returndata: &[0x12, 0x34],
            },
        )));
        tracer.finish_tx();

        let frame = tracer.results.pop().expect("missing tx trace");
        assert_eq!(frame.gas, 55);
        assert!(!frame.failed);
        assert_eq!(frame.return_value, Bytes::from(vec![0x12, 0x34]));
        assert_eq!(frame.struct_logs.len(), 1, "limit should truncate logs");

        let log = &frame.struct_logs[0];
        assert_eq!(log.pc, 7);
        assert_eq!(log.op.as_ref(), "SLOAD");
        assert_eq!(log.gas, 100);
        assert_eq!(log.gas_cost, 3);
        assert_eq!(log.depth, 1);
        assert!(log.stack.is_none(), "stack capture should be disabled");
        assert_eq!(log.memory.as_ref().map(Vec::len), Some(2));
        assert_eq!(log.return_data, Some(Bytes::from(vec![0xbb, 0xcc])));
        assert_eq!(
            log.storage
                .as_ref()
                .and_then(|storage| storage.get(&B256::from([0x33; 32]))),
            Some(&B256::from([0x44; 32]))
        );
    }

    #[test]
    fn reconcile_uses_final_tx_result() {
        let mut frame = DefaultFrame {
            failed: false,
            gas: 1,
            return_value: Bytes::new(),
            struct_logs: vec![],
        };

        reconcile_default_trace_with_output(
            &mut frame,
            &make_tx_output(ExecutionResult::Revert(vec![0xde, 0xad])),
        );
        assert!(frame.failed);
        assert_eq!(frame.gas, 42_000);
        assert_eq!(frame.return_value, Bytes::from(vec![0xde, 0xad]));

        reconcile_default_trace_with_output(
            &mut frame,
            &make_tx_output(ExecutionResult::Success(ExecutionOutput::Call(vec![
                0xca, 0xfe,
            ]))),
        );
        assert!(!frame.failed);
        assert_eq!(frame.return_value, Bytes::from(vec![0xca, 0xfe]));
    }
}
