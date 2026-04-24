//! Policy-owned [`EvmTracer`] that captures per-call-frame data for the
//! `/judge` request body. Lives alongside [`super::PolicyClient`] and shares
//! its scratch state through an `Arc<Mutex<TraceState>>` slot.
//!
//! The shared slot — rather than internal tracer state — is required because
//! the bootloader fires `validator.finish_tx` *before* `tracer.finish_tx`.
//! `PolicyClient::finish_tx` therefore must be able to read the captured
//! frames at the moment it runs, not afterwards.

use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};
use zksync_os_evm_errors::EvmError;
use zksync_os_interface::tracing::{
    AnyTracer, CallModifier, CallResult, EvmFrameInterface, EvmRequest, EvmResources, EvmTracer,
};

/// Per-frame summary captured by [`Tracer`] and shipped to `/judge`.
#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub caller: Address,
    pub callee: Address,
    pub value: U256,
    pub calldata: Vec<u8>,
    pub deploys: Vec<Address>,
}

/// Mutable trace state shared between the tracer (writer) and the consuming
/// `PolicyClient::finish_tx` (reader).
#[derive(Default)]
pub(super) struct TraceState {
    frames: Vec<CapturedFrame>,
    open: Vec<usize>,
}

impl TraceState {
    /// Drain the captured frames and reset the open-frame stack. Called by
    /// `PolicyClient::finish_tx` once per tx.
    pub(super) fn take_frames(&mut self) -> Vec<CapturedFrame> {
        self.open.clear();
        std::mem::take(&mut self.frames)
    }
}

pub(super) type TraceSlot = Arc<Mutex<TraceState>>;

pub(super) fn new_slot() -> TraceSlot {
    Arc::new(Mutex::new(TraceState::default()))
}

/// Tracer paired with a [`super::PolicyClient`] via [`PolicyClient::paired_tracer`].
///
/// Captures only the fields the M3 judge contract requires:
/// per frame, `(caller, callee, value, calldata, deploys)`. `deploys` records
/// the deployed addresses of CREATE/CREATE2 frames opened directly inside
/// this frame. Storage reads/writes and event emissions are deliberately
/// out of scope.
pub struct Tracer {
    slot: TraceSlot,
}

impl Tracer {
    pub(super) fn new(slot: TraceSlot) -> Self {
        Self { slot }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TraceState> {
        // The slot is only ever touched from the block-build task that owns
        // both the tracer and its paired `PolicyClient`; contention is
        // structurally impossible. A poisoned mutex means a prior call
        // panicked while holding the lock — the tx is already dead at that
        // point, fail fast.
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
        let value = request.nominal_token_value();
        let calldata = request.input().to_vec();

        let mut state = self.lock();
        if modifier == CallModifier::Constructor
            && let Some(parent_index) = state.open.last().copied()
        {
            // The opened frame deploys `callee`; record it on the parent.
            // Top-level deployments have no parent and are therefore not
            // recorded as a deploy entry — the recipient sees them as a
            // top-level frame whose `callee` is the deployed address.
            state.frames[parent_index].deploys.push(callee);
        }
        let new_index = state.frames.len();
        state.frames.push(CapturedFrame {
            caller,
            callee,
            value,
            calldata,
            deploys: Vec::new(),
        });
        state.open.push(new_index);
    }

    fn after_execution_frame_completed(&mut self, _result: Option<(EvmResources, CallResult)>) {
        // Frames complete LIFO; drop the topmost open index.
        let mut state = self.lock();
        state.open.pop();
    }

    fn on_storage_read(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_storage_write(&mut self, _: bool, _: Address, _: B256, _: B256) {}
    fn on_bytecode_change(&mut self, _: Address, _: Option<&[u8]>, _: B256, _: u32) {}
    fn on_event(&mut self, _: Address, _: Vec<B256>, _: &[u8]) {}

    fn begin_tx(&mut self, _calldata: &[u8]) {
        let mut state = self.lock();
        state.frames.clear();
        state.open.clear();
    }

    fn finish_tx(&mut self) {
        // The bootloader fires `tracer.finish_tx` *after* `validator.finish_tx`,
        // so by this point `PolicyClient::finish_tx` should already have drained
        // the slot. A non-empty slot here means either no paired validator
        // consumed it or the tx aborted before validator.finish_tx — clear it
        // either way so the next tx starts clean.
        let mut state = self.lock();
        if !state.frames.is_empty() {
            tracing::debug!(
                frames = state.frames.len(),
                "policy tracer dropping unconsumed trace at end of tx"
            );
            state.frames.clear();
        }
        state.open.clear();
    }

    fn before_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn after_evm_interpreter_execution_step(&mut self, _: u8, _: impl EvmFrameInterface) {}
    fn on_opcode_error(&mut self, _: &EvmError, _: impl EvmFrameInterface) {}
    fn on_call_error(&mut self, _: &EvmError) {}
    fn on_selfdestruct(&mut self, _: Address, _: U256, _: impl EvmFrameInterface) {}
    fn on_create_request(&mut self, _: bool) {}
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

        let frames = slot.lock().unwrap().take_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].caller, A);
        assert_eq!(frames[0].callee, B);
        assert_eq!(frames[0].calldata, vec![1, 2, 3]);
        assert_eq!(frames[0].value, U256::from(7));
        assert!(frames[0].deploys.is_empty());
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

        let frames = slot.lock().unwrap().take_frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].deploys, vec![C]);
        // The constructor frame itself records no deploy (it *is* the deploy).
        assert!(frames[1].deploys.is_empty());
    }

    #[test]
    fn top_level_deployment_has_no_parent_deploy_entry() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::Constructor, &[0xcc], 0));
        t.after_execution_frame_completed(None);

        let frames = slot.lock().unwrap().take_frames();
        assert_eq!(frames.len(), 1);
        // Top-level deployment: no parent to record into.
        assert!(frames[0].deploys.is_empty());
    }

    #[test]
    fn begin_tx_clears_residual_state() {
        let (mut t, slot) = pair();
        t.begin_tx(&[]);
        t.on_new_execution_frame(&frame(A, B, CallModifier::NoModifier, &[], 0));
        // No after_execution_frame_completed: simulate a tx that aborted mid-flight.
        t.begin_tx(&[]);
        let state = slot.lock().unwrap();
        assert!(state.frames.is_empty());
        assert!(state.open.is_empty());
    }
}
