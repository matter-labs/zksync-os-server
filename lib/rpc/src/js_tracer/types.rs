use alloy::primitives::U256;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CreateType {
    Create,
    Create2,
}

pub(crate) struct StepCtx {
    pub opcode: u8,
    pub pc: u64,
    pub gas_before: u64,
    pub depth: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TxContext {
    pub typ: String,
    pub from: alloy::primitives::Address,
    pub to: alloy::primitives::Address,
    pub input: alloy::primitives::Bytes,
    pub gas: U256,
    pub value: U256,

    // the fields below are only filled during when the frame is exited
    pub gas_used: Option<U256>,
    pub output: Option<alloy::primitives::Bytes>,
    pub error: Option<String>,
}
