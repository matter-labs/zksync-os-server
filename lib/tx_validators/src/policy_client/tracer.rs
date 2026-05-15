//! `EvmTracer` that captures per-frame data for [`super::PolicyClient`].
//! The captured frames live in an `Arc<Mutex<TraceState>>` slot shared
//! with the client (the tracer writes; the validator hook reads).

use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};
use serde::Serialize;
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmTracer,
};

/// Per-frame call classification. Coarser than upstream
/// [`zksync_os_interface::tracing::CallModifier`] (CALLCODE collapses
/// into DelegateCall, system-static into StaticCall, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CallKind {
    Call,
    DelegateCall,
    StaticCall,
    Constructor,
}

impl From<CallModifier> for CallKind {
    fn from(modifier: CallModifier) -> Self {
        match modifier {
            // ZKVMSystem is protocol-internal; map to the closest user-visible kind.
            CallModifier::NoModifier | CallModifier::ZKVMSystem => Self::Call,
            CallModifier::Constructor => Self::Constructor,
            // CALLCODE and DelegateStatic share delegatecall's storage-context semantics.
            CallModifier::Delegate
            | CallModifier::EVMCallcode
            | CallModifier::EVMCallcodeStatic
            | CallModifier::DelegateStatic => Self::DelegateCall,
            CallModifier::Static | CallModifier::ZKVMSystemStatic => Self::StaticCall,
        }
    }
}

/// Per-frame summary captured by [`Tracer`] and shipped to `/judge`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedFrame {
    pub caller: Address,
    pub callee: Address,
    pub value: U256,
    #[serde(with = "alloy::hex")]
    pub calldata: Vec<u8>,
    pub deploys: Vec<Address>,
    pub call_kind: CallKind,
    pub children: Vec<CapturedFrame>,
}

/// Mutable trace state shared between the tracer (writer) and the consuming
/// `PolicyClient::finish_tx` (reader).
#[derive(Default)]
pub(super) struct TraceState {
    /// Stack of open (not yet completed) frames, innermost last.
    frame_stack: Vec<CapturedFrame>,
    /// The single root frame, set when the outermost frame completes.
    root: Option<CapturedFrame>,
}

impl TraceState {
    pub(super) fn take_root(&mut self) -> Option<CapturedFrame> {
        self.frame_stack.clear();
        std::mem::take(&mut self.root)
    }
}

pub(super) type TraceSlot = Arc<Mutex<TraceState>>;

pub(super) fn new_slot() -> TraceSlot {
    Arc::new(Mutex::new(TraceState::default()))
}

/// Captures `(caller, callee, value, calldata, deploys, call_kind)` per
/// frame. `deploys` lists the deployed addresses of CREATE/CREATE2 frames
/// opened directly inside this frame. Storage reads/writes and events
/// are out of scope. Always paired with a [`super::PolicyClient`].
#[repr(transparent)]
pub struct Tracer {
    slot: TraceSlot,
}

impl Tracer {
    pub(super) fn new(slot: TraceSlot) -> Self {
        Self { slot }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TraceState> {
        self.slot.lock().expect("policy tracer slot mutex poisoned")
    }
}

impl AnyTracer for Tracer {
    fn as_evm(&mut self) -> Option<&mut impl EvmTracer> {
        Some(self)
    }
}

impl EvmTracer for Tracer {
    fn on_new_execution_frame(&mut self, request: impl EvmRequest) {
        let caller = request.caller();
        let callee = request.callee();
        let modifier = request.modifier();
        // Static-flavored frames cannot transfer value; report zero rather
        // than whatever the EVM left in the slot. Matches `CallTracer`'s
        // None-for-STATICCALL behaviour.
        let value = if is_static(modifier) {
            U256::ZERO
        } else {
            request.nominal_token_value()
        };
        let calldata = request.input().to_vec();

        let mut state = self.lock();
        if modifier == CallModifier::Constructor {
            // Record the deployed address on the parent frame. Top-level
            // deployments have no parent; the recipient sees them as a
            // top-level frame whose `callee` is the deployed address.
            if let Some(parent) = state.frame_stack.last_mut() {
                parent.deploys.push(callee);
            }
        }
        state.frame_stack.push(CapturedFrame {
            caller,
            callee,
            value,
            calldata,
            deploys: Vec::new(),
            call_kind: CallKind::from(modifier),
            children: Vec::new(),
        });
    }

    fn after_execution_frame_completed(&mut self, _result: Option<(EvmResources, CallResult)>) {
        let mut state = self.lock();
        if let Some(completed) = state.frame_stack.pop() {
            if let Some(parent) = state.frame_stack.last_mut() {
                parent.children.push(completed);
            } else {
                state.root = Some(completed);
            }
        }
    }

    fn on_storage_read(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_storage_write(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_bytecode_change(&mut self, _: Address, _: Option<&[u8]>, _: B256, _: u32) {}
    fn on_event(&mut self, _: Address, _: Vec<B256>, _: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        let mut state = self.lock();
        state.frame_stack.clear();
        state.root = None;
    }

    fn finish_tx(&mut self) {
        // Leave the completed root in the slot for `PolicyClient::finish_tx`
        // to drain. Only clear the open stack so a tx that aborted mid-frame
        // doesn't mis-parent the next tx's first frame.
        let mut state = self.lock();
        state.frame_stack.clear();
    }

    fn before_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn after_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn on_opcode_error(&mut self, _: &EvmError, _: impl EvmFrameInterface) {}
    fn on_call_error(&mut self, _: &EvmError) {}
    fn on_selfdestruct(&mut self, _: Address, _: U256, _: impl EvmFrameInterface) {}
    fn on_create_request(&mut self, _: bool) {}
}

fn is_static(modifier: CallModifier) -> bool {
    matches!(
        modifier,
        CallModifier::Static
            | CallModifier::DelegateStatic
            | CallModifier::EVMCallcodeStatic
            | CallModifier::ZKVMSystemStatic
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const A: Address = address!("0x1111111111111111111111111111111111111111");
    const B: Address = address!("0x2222222222222222222222222222222222222222");
    const C: Address = address!("0x3333333333333333333333333333333333333333");

    struct MockRequest {
        caller: Address,
        callee: Address,
        modifier: CallModifier,
        input: Vec<u8>,
        value: U256,
    }

    impl EvmRequest for &MockRequest {
        fn resources(&self) -> EvmResources {
            EvmResources::default()
        }
        fn caller(&self) -> Address {
            self.caller
        }
        fn callee(&self) -> Address {
            self.callee
        }
        fn modifier(&self) -> CallModifier {
            self.modifier
        }
        fn input(&self) -> &[u8] {
            &self.input
        }
        fn nominal_token_value(&self) -> U256 {
            self.value
        }
    }

    fn frame(
        caller: Address,
        callee: Address,
        modifier: CallModifier,
        input: &[u8],
        value: u64,
    ) -> MockRequest {
        MockRequest {
            caller,
            callee,
            modifier,
            input: input.to_vec(),
            value: U256::from(value),
        }
    }

    fn pair() -> (Tracer, TraceSlot) {
        let slot = new_slot();
        (Tracer::new(slot.clone()), slot)
    }

    #[test]
    fn captures_single_top_level_frame() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::NoModifier, &[1, 2, 3], 7));
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        assert_eq!(root.caller, A);
        assert_eq!(root.callee, B);
        assert_eq!(root.calldata, vec![1, 2, 3]);
        assert_eq!(root.value, U256::from(7));
        assert!(root.deploys.is_empty());
        assert!(root.children.is_empty());
        assert_eq!(root.call_kind, CallKind::Call);
    }

    #[test]
    fn nested_constructor_records_deploy_on_parent() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        // Outer call EOA -> Factory.
        t.on_new_execution_frame(&frame(A, B, CallModifier::NoModifier, &[0xaa], 0));
        // Factory deploys C via CREATE.
        t.on_new_execution_frame(&frame(B, C, CallModifier::Constructor, &[0xbb], 0));
        t.after_execution_frame_completed(None);
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        assert_eq!(root.deploys, vec![C]);
        assert_eq!(root.call_kind, CallKind::Call);
        assert_eq!(root.children.len(), 1);
        // The constructor frame itself records no deploy (it *is* the deploy)
        // but does carry the Constructor call_kind.
        assert!(root.children[0].deploys.is_empty());
        assert_eq!(root.children[0].call_kind, CallKind::Constructor);
    }

    #[test]
    fn delegatecall_frame_records_call_kind() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        // EOA -> Proxy.
        t.on_new_execution_frame(&frame(A, B, CallModifier::NoModifier, &[0xaa], 0));
        // Proxy delegatecalls into Impl.
        t.on_new_execution_frame(&frame(B, C, CallModifier::Delegate, &[0xbb], 0));
        t.after_execution_frame_completed(None);
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        assert_eq!(root.call_kind, CallKind::Call);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].call_kind, CallKind::DelegateCall);
    }

    #[test]
    fn staticcall_frame_records_call_kind() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::Static, &[0xab], 0));
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        assert_eq!(root.call_kind, CallKind::StaticCall);
    }

    #[test]
    fn callcode_collapses_to_delegatecall() {
        // CALLCODE preserves the caller's storage context, like DELEGATECALL.
        // The service treats them identically, so the wire kind is
        // DelegateCall rather than introducing a separate CallKind variant.
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::EVMCallcode, &[0xcd], 0));
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        assert_eq!(root.call_kind, CallKind::DelegateCall);
    }

    #[test]
    fn top_level_deployment_has_no_parent_deploy_entry() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::Constructor, &[0xcc], 0));
        t.after_execution_frame_completed(None);

        let root = slot.lock().unwrap().take_root().expect("expected root");
        // Top-level deployment: no parent to record into.
        assert!(root.deploys.is_empty());
    }

    #[test]
    fn begin_tx_clears_residual_state() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::NoModifier, &[], 0));
        // No after_execution_frame_completed: simulate a tx that aborted mid-flight.
        t.begin_tx(&[]);
        let state = slot.lock().unwrap();
        assert!(state.frame_stack.is_empty());
        assert!(state.root.is_none());
    }
}
