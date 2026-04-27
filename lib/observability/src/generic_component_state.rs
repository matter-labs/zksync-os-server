use vise::EncodeLabelValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum GenericComponentState {
    /// No work available — waiting for upstream.
    Idle,
    /// Actively processing.
    Active,
}

impl GenericComponentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
        }
    }
}
