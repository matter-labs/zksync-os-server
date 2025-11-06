use alloy::primitives::{Address, B256, Bytes, I256, U256};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CreateType {
    Create,
    Create2,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TracerMethod {
    Setup,
    Enter,
    Exit,
    Step,
    Fault,
    Result,
    Write,
}

impl TracerMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            TracerMethod::Setup => "setup",
            TracerMethod::Enter => "enter",
            TracerMethod::Exit => "exit",
            TracerMethod::Step => "step",
            TracerMethod::Fault => "fault",
            TracerMethod::Result => "result",
            TracerMethod::Write => "write",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OverlayEntry<V> {
    pub(crate) value: V,
    pub(crate) committed: bool,
    pub(crate) previous: Option<V>,
}

impl<V> OverlayEntry<V> {
    pub(crate) fn new_pending(value: V) -> Self {
        Self {
            value,
            committed: false,
            previous: None,
        }
    }
}

pub type StorageOverlay = HashMap<(Address, B256), OverlayEntry<B256>>;
pub type CodeOverlay = HashMap<Address, OverlayEntry<Option<Vec<u8>>>>;
pub type BalanceOverlay = HashMap<Address, OverlayEntry<I256>>;

pub(crate) struct StepCtx {
    pub opcode: u8,
    pub pc: u64,
    pub gas_before: u64,
    pub depth: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TxContext {
    pub typ: String,
    pub from: Address,
    pub to: Address,
    pub input: Bytes,
    pub gas: U256,
    pub value: U256,

    // the fields below are only filled during when the frame is exited
    pub gas_used: Option<U256>,
    pub output: Option<Bytes>,
    pub error: Option<String>,
}
