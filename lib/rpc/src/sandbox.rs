use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::trace::geth::{CallConfig, CallFrame, CallLogFrame};
use zk_ee::system::evm::EvmFrameInterface;
use zk_ee::system::evm::errors::EvmError;
use zk_ee::system::tracer::evm_tracer::EvmTracer;
use zk_ee::system::tracer::{NopTracer, Tracer};
use zk_ee::system::{
    CallModifier, CallResult, EthereumLikeTypes, ExecutionEnvironmentLaunchParams, Resources,
    SystemTypes,
};
use zk_ee::types_config::SystemIOTypesConfig;
use zk_os_evm_interpreter::ERGS_PER_GAS;
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
    pub transactions: Vec<CallFrame>,
    pub unfinished_calls: Vec<CallFrame>,
    pub finished_calls: Vec<CallFrame>,
    pub current_call_depth: usize,
    pub collect_logs: bool,
    pub only_top_call: bool,
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
        }
    }
}

impl<S: EthereumLikeTypes> Tracer<S> for CallTracer {
    fn on_new_execution_frame(&mut self, initial_state: &ExecutionEnvironmentLaunchParams<S>) {
        self.current_call_depth += 1;

        if !self.only_top_call || self.current_call_depth == 1 {
            self.unfinished_calls.push(CallFrame {
                from: Address::from(initial_state.external_call.caller.to_be_bytes()),
                gas: U256::from(
                    initial_state.external_call.available_resources.ergs().0 / ERGS_PER_GAS,
                ),
                gas_used: U256::ZERO, // will be populated later
                to: if initial_state.external_call.callee == ruint::aliases::B160::ZERO {
                    None
                } else {
                    Some(Address::from(
                        initial_state.external_call.callee.to_be_bytes(),
                    ))
                },
                input: Bytes::copy_from_slice(initial_state.external_call.input),
                output: None,        // will be populated later
                error: None,         // can be populated later
                revert_reason: None, // can be populated later
                calls: vec![],       // will be populated later
                logs: vec![],        // will be populated later
                value: if initial_state.external_call.nominal_token_value == U256::ZERO {
                    None
                } else {
                    Some(initial_state.external_call.nominal_token_value)
                },
                typ: match initial_state.external_call.modifier {
                    CallModifier::NoModifier => "CALL",
                    CallModifier::Constructor => "",
                    CallModifier::Delegate => "DELEGATECALL",
                    CallModifier::Static => "STATICCALL",
                    // fixme: is this really a custom call type?
                    CallModifier::DelegateStatic => "DELEGATESTATICCALL",
                    CallModifier::ZKVMSystem => {
                        panic!("unexpected call type: ZKVMSystem")
                    }
                    CallModifier::ZKVMSystemStatic => {
                        panic!("unexpected call type: ZKVMSystemStatic")
                    }
                    CallModifier::EVMCallcode => "CALLCODE",
                    // fixme: is this really a custom call type?
                    CallModifier::EVMCallcodeStatic => "CALLCODESTATIC",
                }
                .to_string(),
            })
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
                            // todo: decode error?
                            finished_call.revert_reason = Some("Unknown reason".to_string());
                            finished_call.output =
                                Some(Bytes::copy_from_slice(return_values.returndata));
                        }
                        CallResult::Successful { return_values } => {
                            finished_call.output =
                                Some(Bytes::copy_from_slice(return_values.returndata));
                        }
                    };
                }
                None => {
                    // Some unexpected internal failure happened (maybe out of native resources)
                    // Should revert whole tx
                    finished_call.gas_used = finished_call.gas;
                    // todo: decode error?
                    finished_call.revert_reason = Some("Unknown reason".to_string());
                    finished_call.error = Some("Internal error".to_string());
                }
            }
            if let Some(parent_call) = self.unfinished_calls.last_mut() {
                parent_call.calls.push(finished_call);
            } else {
                self.finished_calls.push(finished_call);
            }
        }

        self.current_call_depth -= 1;
    }

    fn begin_tx(&mut self, _calldata: &[u8]) {
        self.current_call_depth = 0;
    }

    fn finish_tx(&mut self) {
        assert_eq!(self.current_call_depth, 0);
        assert!(self.unfinished_calls.is_empty());
        assert_eq!(self.finished_calls.len(), 1);

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

    #[inline(always)]
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
                            .into_iter()
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
        current_call.error = Some(format!("{:?}", error));
        // todo: derive from `error`
        current_call.revert_reason = Some("Unknown reason".to_string());
    }

    /// Special cases, when error happens in frame before any opcode is executed (unfortunately we can't provide access to state)
    /// Note: call frame ends immediately
    fn on_call_error(&mut self, error: &EvmError) {
        let current_call = self.unfinished_calls.last_mut().expect("Should exist");
        current_call.error = Some(format!("{:?}", error));
        // todo: derive from `error`
        current_call.revert_reason = Some("Unknown reason".to_string());
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
            to: if beneficiary == ruint::aliases::B160::ZERO {
                None
            } else {
                Some(Address::from(beneficiary.to_be_bytes()))
            },
            input: Default::default(),
            output: None,
            error: None,
            revert_reason: None,
            calls: vec![],
            logs: vec![],
            value: if token_value == U256::ZERO {
                None
            } else {
                Some(token_value)
            },
            typ: "SELFDESTRUCT".to_string(),
        };

        if let Some(parent_call) = self.unfinished_calls.last_mut() {
            parent_call.calls.push(call_frame);
        } else {
            self.finished_calls.push(call_frame);
        }
    }
}
