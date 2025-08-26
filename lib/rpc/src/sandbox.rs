use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::trace::geth::{CallConfig, CallFrame, CallLogFrame};
use alloy::sol_types::{ContractError, GenericRevertReason};
use zk_ee::system::evm::EvmFrameInterface;
use zk_ee::system::evm::errors::EvmError;
use zk_ee::system::tracer::evm_tracer::EvmTracer;
use zk_ee::system::tracer::{NopTracer, Tracer};
use zk_ee::system::{
    CallModifier, CallResult, EthereumLikeTypes, ExecutionEnvironmentLaunchParams, Resources,
    SystemTypes,
};
use zk_ee::types_config::SystemIOTypesConfig;
use zk_os_evm_interpreter::{ERGS_PER_GAS, STACK_SIZE};
use zk_os_forward_system::run::errors::ForwardSubsystemError;
use zk_os_forward_system::run::output::TxResult;
use zk_os_forward_system::run::{BlockContext, simulate_tx};
use zksync_os_state::StateView;
use zksync_os_types::{L2Transaction, ZkTransaction, ZksyncOsEncode};

pub fn execute(
    tx: L2Transaction,
    mut block_context: BlockContext,
    state_view: StateView,
) -> Result<TxResult, Box<ForwardSubsystemError>> {
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut NopTracer::default(),
    )
    .map_err(Box::new)
}

pub fn call_trace(
    tx: ZkTransaction,
    mut block_context: BlockContext,
    state_view: StateView,
    call_config: CallConfig,
) -> Result<CallFrame, Box<ForwardSubsystemError>> {
    let encoded_tx = tx.encode();

    block_context.eip1559_basefee = U256::from(0);

    let mut tracer = CallTracer::new_with_config(
        call_config.with_log.unwrap_or_default(),
        call_config.only_top_call.unwrap_or_default(),
    );
    let _ = simulate_tx(
        encoded_tx,
        block_context,
        state_view.clone(),
        state_view,
        &mut tracer,
    )
    .map_err(Box::new)?;
    Ok(tracer
        .transactions
        .pop()
        .expect("tracer should have at least one transaction"))
}

#[derive(Default)]
pub struct CallTracer {
    transactions: Vec<CallFrame>,
    unfinished_calls: Vec<CallFrame>,
    finished_calls: Vec<CallFrame>,
    current_call_depth: usize,
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
    pub fn new_with_config(collect_logs: bool, only_top_call: bool) -> Self {
        Self {
            transactions: vec![],
            unfinished_calls: vec![],
            finished_calls: vec![],
            current_call_depth: 0,
            collect_logs,
            only_top_call,
            create_operation_requested: None,
        }
    }
}

impl<S: EthereumLikeTypes> Tracer<S> for CallTracer {
    fn on_new_execution_frame(&mut self, initial_state: &ExecutionEnvironmentLaunchParams<S>) {
        self.current_call_depth += 1;

        if !self.only_top_call || self.current_call_depth == 1 {
            // Top-level deployment (initiated by EOA) won't trigger `on_create_request` hook
            // This is always a CREATE
            if self.current_call_depth == 1
                && initial_state.external_call.modifier == CallModifier::Constructor
            {
                self.create_operation_requested = Some(CreateType::Create);
            }

            self.unfinished_calls.push(CallFrame {
                from: Address::from(initial_state.external_call.caller.to_be_bytes()),
                gas: U256::from(
                    initial_state.external_call.available_resources.ergs().0 / ERGS_PER_GAS,
                ),
                gas_used: U256::ZERO, // will be populated later
                to: Some(Address::from(
                    initial_state.external_call.callee.to_be_bytes(),
                )),
                input: Bytes::copy_from_slice(initial_state.external_call.input),
                output: None,        // will be populated later
                error: None,         // can be populated later
                revert_reason: None, // can be populated later
                calls: vec![],       // will be populated later
                logs: vec![],        // will be populated later
                value: if initial_state.external_call.modifier == CallModifier::Static {
                    // STATICCALL frames don't have `value`
                    None
                } else {
                    Some(initial_state.external_call.nominal_token_value)
                },
                typ: match initial_state.external_call.modifier {
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

    fn after_execution_frame_completed(&mut self, result: Option<(&S::Resources, &CallResult<S>)>) {
        assert_ne!(self.current_call_depth, 0);

        if !self.only_top_call || self.current_call_depth == 1 {
            let mut finished_call = self.unfinished_calls.pop().expect("Should exist");

            match result {
                Some(result) => {
                    finished_call.gas_used = finished_call
                        .gas
                        .saturating_sub(U256::from(result.0.ergs().0 / ERGS_PER_GAS));

                    match &result.1 {
                        CallResult::PreparationStepFailed => {
                            panic!("Should not happen") // ZKsync OS should not call tracer in this case
                        }
                        CallResult::Failed { return_values } => {
                            finished_call.revert_reason =
                                maybe_revert_reason(return_values.returndata);
                            finished_call.output =
                                Some(Bytes::copy_from_slice(return_values.returndata));
                            if finished_call.typ == "CREATE" || finished_call.typ == "CREATE2" {
                                // Clear `to` field as no contract was created
                                finished_call.to = None;
                            }
                        }
                        CallResult::Successful { return_values } => {
                            if finished_call.typ == "CREATE" || finished_call.typ == "CREATE2" {
                                // output should be already populated in `on_bytecode_change` hook
                            } else {
                                finished_call.output =
                                    Some(Bytes::copy_from_slice(return_values.returndata));
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
                }
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

        // Sanity check
        assert!(self.create_operation_requested.is_none());
    }

    fn finish_tx(&mut self) {
        assert_eq!(self.current_call_depth, 0);
        assert!(self.unfinished_calls.is_empty());
        assert_eq!(self.finished_calls.len(), 1);

        // Sanity check
        assert!(self.create_operation_requested.is_none());

        self.transactions
            .push(self.finished_calls.pop().expect("Should exist"));
    }

    #[inline(always)]
    fn on_storage_read(
        &mut self,
        _ee_type: zk_ee::execution_environment_type::ExecutionEnvironmentType,
        _is_transient: bool,
        _address: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::Address,
        _key: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::StorageKey,
        _value: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::StorageValue,
    ) {
    }

    #[inline(always)]
    fn on_storage_write(
        &mut self,
        _ee_type: zk_ee::execution_environment_type::ExecutionEnvironmentType,
        _is_transient: bool,
        _address: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::Address,
        _key: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::StorageKey,
        _value: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::StorageValue,
    ) {
    }

    fn on_event(
        &mut self,
        _ee_type: zk_ee::execution_environment_type::ExecutionEnvironmentType,
        address: &<<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::Address,
        topics: &[<<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::EventKey],
        data: &[u8],
    ) {
        if self.collect_logs {
            let call = self.unfinished_calls.last_mut().expect("Should exist");
            call.logs.push(CallLogFrame {
                address: if address == &ruint::aliases::B160::ZERO {
                    None
                } else {
                    Some(Address::from(address.to_be_bytes()))
                },
                topics: if topics.is_empty() {
                    None
                } else {
                    Some(
                        topics
                            .iter()
                            .map(|topic| B256::new(topic.as_u8_array()))
                            .collect(),
                    )
                },
                data: if data.is_empty() {
                    None
                } else {
                    Some(Bytes::copy_from_slice(data))
                },
                // todo: populate
                position: None,
            })
        }
    }

    fn on_bytecode_change(
        &mut self,
        _ee_type: zk_ee::execution_environment_type::ExecutionEnvironmentType,
        address: <S::IOTypes as SystemIOTypesConfig>::Address,
        new_bytecode: Option<&[u8]>,
        _new_bytecode_hash: <S::IOTypes as SystemIOTypesConfig>::BytecodeHashValue,
        new_observable_bytecode_length: u32,
    ) {
        let call = self.unfinished_calls.last_mut().expect("Should exist");

        if call.typ == "CREATE" || call.typ == "CREATE2" {
            assert_eq!(
                Address::from(address.to_be_bytes()),
                call.to.expect("Should exist")
            );
            let deployed_raw_bytecode = new_bytecode.expect("Should be present");

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
    fn evm_tracer(&mut self) -> &mut impl EvmTracer<S> {
        self
    }
}

impl<S: EthereumLikeTypes> EvmTracer<S> for CallTracer {
    #[inline(always)]
    fn before_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        _interpreter_state: &impl EvmFrameInterface<S>,
    ) {
    }

    #[inline(always)]
    fn after_evm_interpreter_execution_step(
        &mut self,
        _opcode: u8,
        _interpreter_state: &impl EvmFrameInterface<S>,
    ) {
    }

    /// Opcode failed for some reason. Note: call frame ends immediately
    fn on_opcode_error(&mut self, error: &EvmError, _frame_state: &impl EvmFrameInterface<S>) {
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
        let current_call = self.unfinished_calls.last_mut().expect("Should exist");
        current_call.error = Some(fmt_error_msg(error));

        // Sanity check
        assert!(self.create_operation_requested.is_none());
    }

    /// We should treat selfdestruct as a special kind of a call
    fn on_selfdestruct(
        &mut self,
        beneficiary: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::Address,
        token_value: <<S as SystemTypes>::IOTypes as SystemIOTypesConfig>::NominalTokenValue,
        frame_state: &impl EvmFrameInterface<S>,
    ) {
        // Following Geth implementation: https://github.com/ethereum/go-ethereum/blob/2dbb580f51b61d7ff78fceb44b06835827704110/core/vm/instructions.go#L894
        let call_frame = CallFrame {
            from: Address::from(frame_state.address().to_be_bytes()),
            gas: Default::default(),
            gas_used: Default::default(),
            // todo: consider returning `None` here as this is only `Some` if a selfdestruct was
            //       executed and the call is executed before the Cancun hardfork.
            to: Some(Address::from(beneficiary.to_be_bytes())),
            input: Default::default(),
            output: None,
            error: None,
            revert_reason: None,
            calls: vec![],
            logs: vec![],
            // todo: consider returning `None` here as this is only `Some` if a selfdestruct was
            //       executed and the call is executed before the Cancun hardfork.
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
fn maybe_revert_reason(output: &[u8]) -> Option<String> {
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
fn fmt_error_msg(error: &EvmError) -> String {
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
        // todo: check that both variants below accurately map to `ErrWriteProtection` from geth
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
