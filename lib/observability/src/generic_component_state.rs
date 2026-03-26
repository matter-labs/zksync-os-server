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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_covers_all_variants() {
        assert_eq!(GenericComponentState::Idle.as_str(), "idle");
        assert_eq!(GenericComponentState::Active.as_str(), "active");
        assert_eq!(GenericComponentState::Throttled.as_str(), "throttled");
    }
}
