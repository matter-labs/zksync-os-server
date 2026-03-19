use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, B256, U256, address};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTracer, AnyTxValidator, CallModifier, CallResult, EvmFrameInterface, EvmRequest,
    EvmResources, EvmTracer, TxValidationResult, TxValidator,
};

/// Always allowed to deploy — ensures protocol upgrades are never blocked by the filter.
const FORCE_DEPLOYER_ADDRESS: Address = address!("0000000000000000000000000000000000008007");

#[derive(Clone, Debug, Default)]
pub enum Config {
    #[default]
    Unrestricted,
    AllowList(HashSet<Address>),
}

impl Config {
    pub fn allow_list(addresses: impl IntoIterator<Item = Address>) -> Self {
        Self::AllowList(addresses.into_iter().collect())
    }
}

/// Detects unauthorized deployments by checking `tx.origin` against the allow-list.
///
/// Uses `tx.origin` (not `msg.sender`) so the check applies even to indirect deploys
/// through factory contracts.
///
/// Tracer and Validator are separate structs because `run_block` takes them as separate
/// `&mut` parameters — a single struct can't be passed as both. They communicate via
/// a shared `Arc<AtomicBool>`.
pub struct Tracer {
    unauthorized_deployment: Arc<AtomicBool>,
    config: Config,
    tx_origin: OnceCell<Address>,
}

impl Tracer {
    pub fn new(unauthorized_deployment: Arc<AtomicBool>, config: Config) -> Self {
        Self {
            unauthorized_deployment,
            config,
            tx_origin: OnceCell::new(),
        }
    }
}

impl AnyTracer for Tracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl Tracer {
    fn should_reject(&self, modifier: CallModifier, tx_origin: &Address) -> bool {
        if modifier != CallModifier::Constructor {
            return false;
        }
        if tx_origin == &FORCE_DEPLOYER_ADDRESS {
            return false;
        }
        let Config::AllowList(allowed) = &self.config else {
            return false;
        };
        !allowed.contains(tx_origin)
    }
}

impl EvmTracer for Tracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        let msg_sender = request.caller();
        let tx_origin = self.tx_origin.get_or_init(|| msg_sender);

        if self.should_reject(request.modifier(), tx_origin) {
            tracing::warn!(
                tx_origin = %tx_origin,
                msg_sender = %msg_sender,
                "Deployment rejected: tx.origin is not in the allow-list"
            );
            self.unauthorized_deployment.store(true, Ordering::Release);
        }
    }

    fn after_execution_frame_completed(&mut self, _result: Option<(EvmResources, CallResult)>) {}
    fn on_storage_read(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_storage_write(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_bytecode_change(&mut self, _: Address, _: Option<&[u8]>, _: B256, _: u32) {}
    fn on_event(&mut self, _: Address, _: Vec<B256>, _: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        // Reset state at the start of each transaction.
        self.tx_origin = OnceCell::new();
        self.unauthorized_deployment.store(false, Ordering::Release);
    }

    fn finish_tx(&mut self) {}
    fn before_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn after_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn on_opcode_error(&mut self, _: &EvmError, _: impl EvmFrameInterface) {}
    fn on_call_error(&mut self, _: &EvmError) {}
    fn on_selfdestruct(&mut self, _: Address, _: U256, _: impl EvmFrameInterface) {}
    fn on_create_request(&mut self, _: bool) {}
}

/// Rejects transactions flagged by [`Tracer`] as containing unauthorized deployments.
pub struct Validator {
    unauthorized_deployment: Arc<AtomicBool>,
}

impl Validator {
    pub fn new(unauthorized_deployment: Arc<AtomicBool>) -> Self {
        Self {
            unauthorized_deployment,
        }
    }
}

impl AnyTxValidator for Validator {
    fn as_evm(&mut self) -> Option<&mut impl TxValidator> {
        Some(self)
    }
}

impl TxValidator for Validator {
    fn finish_tx(&mut self) -> TxValidationResult {
        if self.unauthorized_deployment.load(Ordering::Acquire) {
            return Err(InvalidTransaction::FilteredByValidator);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHORIZED: Address = address!("0x1111111111111111111111111111111111111111");
    const UNAUTHORIZED: Address = address!("0x2222222222222222222222222222222222222222");
    const FACTORY: Address = address!("0x3333333333333333333333333333333333333333");

    /// Minimal `EvmRequest` implementation for testing.
    struct MockRequest {
        caller: Address,
        modifier: CallModifier,
    }

    impl EvmRequest for MockRequest {
        fn resources(&self) -> EvmResources {
            EvmResources::default()
        }
        fn caller(&self) -> Address {
            self.caller
        }
        fn callee(&self) -> Address {
            Address::ZERO
        }
        fn modifier(&self) -> CallModifier {
            self.modifier
        }
        fn input(&self) -> &[u8] {
            &[]
        }
        fn nominal_token_value(&self) -> U256 {
            U256::ZERO
        }
    }

    /// Test harness that wires up a Tracer + Validator with a given config.
    struct Harness {
        tracer: Tracer,
        validator: Validator,
    }

    impl Harness {
        fn new(config: Config) -> Self {
            let flag = Arc::new(AtomicBool::new(false));
            let tracer = Tracer::new(flag.clone(), config);
            let validator = Validator::new(flag);
            Self { tracer, validator }
        }

        /// Simulate a new transaction boundary.
        fn begin_tx(&mut self) {
            EvmTracer::begin_tx(&mut self.tracer, &[]);
            TxValidator::begin_tx(&mut self.validator, &[]).unwrap();
        }

        /// Simulate a frame entering execution.
        fn frame(&mut self, caller: Address, modifier: CallModifier) {
            let request = MockRequest { caller, modifier };
            self.tracer.on_new_execution_frame(request);
        }

        /// Finish the transaction and return the validator result.
        fn finish_tx(&mut self) -> TxValidationResult {
            self.validator.finish_tx()
        }
    }

    #[test]
    fn unauthorized_direct_deploy_is_rejected() {
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        // First frame sets tx_origin = UNAUTHORIZED; Constructor check rejects.
        h.frame(UNAUTHORIZED, CallModifier::Constructor);
        assert!(matches!(
            h.finish_tx(),
            Err(InvalidTransaction::FilteredByValidator)
        ));
    }

    #[test]
    fn allowed_address_constructor_is_accepted() {
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        h.frame(AUTHORIZED, CallModifier::Constructor);
        assert!(h.finish_tx().is_ok());
    }

    #[test]
    fn authorized_eoa_can_deploy_through_factory() {
        // Authorized EOA calls a factory contract which then issues CREATE.
        // The tx_sender is the authorized EOA, so the factory's Constructor
        // frame should be allowed even though the factory isn't in the list.
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        // First frame: EOA → Factory (regular call). Sets tx_sender = AUTHORIZED.
        h.frame(AUTHORIZED, CallModifier::NoModifier);
        // Second frame: Factory → new contract (constructor). Caller is FACTORY,
        // but tx_sender is AUTHORIZED, so this should pass.
        h.frame(FACTORY, CallModifier::Constructor);
        assert!(h.finish_tx().is_ok());
    }

    #[test]
    fn unauthorized_eoa_rejected_even_through_factory() {
        // Unauthorized EOA calls a factory which issues CREATE.
        // tx_sender is unauthorized, so the deploy is rejected.
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        h.frame(UNAUTHORIZED, CallModifier::NoModifier);
        h.frame(FACTORY, CallModifier::Constructor);
        assert!(matches!(
            h.finish_tx(),
            Err(InvalidTransaction::FilteredByValidator)
        ));
    }

    #[test]
    fn non_constructor_frames_are_ignored() {
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        h.frame(UNAUTHORIZED, CallModifier::NoModifier);
        h.frame(UNAUTHORIZED, CallModifier::Delegate);
        h.frame(UNAUTHORIZED, CallModifier::Static);
        assert!(h.finish_tx().is_ok());
    }

    #[test]
    fn force_deployer_is_always_allowed() {
        // FORCE_DEPLOYER_ADDRESS must be able to deploy even when not in the allow-list,
        // so that protocol upgrade transactions are never blocked by the filter.
        let mut h = Harness::new(Config::allow_list(vec![AUTHORIZED]));
        h.begin_tx();
        h.frame(FORCE_DEPLOYER_ADDRESS, CallModifier::Constructor);
        assert!(h.finish_tx().is_ok());
    }

    #[test]
    fn unrestricted_config_allows_everything() {
        let mut h = Harness::new(Config::Unrestricted);
        h.begin_tx();
        h.frame(UNAUTHORIZED, CallModifier::Constructor);
        assert!(h.finish_tx().is_ok());
    }
}
