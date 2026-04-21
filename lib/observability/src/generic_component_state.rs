use vise::EncodeLabelValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum GenericComponentState {
    /// No work available — waiting for upstream to produce more.
    Idle,
    /// Actively processing an item.
    Active,
    /// Has work queued but blocked on an external resource
    /// (e.g. L1 confirmation, prover job slots, service reconnect).
    Throttled,
}

impl GenericComponentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Throttled => "throttled",
        }
    }
}
