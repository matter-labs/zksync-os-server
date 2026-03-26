use crate::GenericComponentState;

/// Maps a component-specific state enum to a generic pipeline state
/// plus a fine-grained label string for Prometheus.
///
/// Implement this for each component's custom state enum. The `generic`
/// value is used for cross-pipeline comparison (backpressure, block diffs).
/// The `specific` string appears as `specific_state` in the
/// `component_time_spent_in_state` metric for detailed debugging.
pub trait StateLabel: Send + 'static {
    fn generic(&self) -> GenericComponentState;
    fn specific(&self) -> &'static str;
}

/// `GenericComponentState` implements `StateLabel` so callers that don't
/// need a custom enum can pass it directly to `enter_state`.
impl StateLabel for GenericComponentState {
    fn generic(&self) -> GenericComponentState {
        *self
    }
    fn specific(&self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_state_implements_state_label() {
        let s = GenericComponentState::Active;
        assert_eq!(s.generic(), GenericComponentState::Active);
        assert_eq!(s.specific(), "active");
    }

    #[test]
    fn all_generic_variants_round_trip() {
        for (state, expected) in [
            (GenericComponentState::Idle, "idle"),
            (GenericComponentState::Active, "active"),
            (GenericComponentState::Throttled, "throttled"),
        ] {
            assert_eq!(state.specific(), expected);
            assert_eq!(state.generic(), state);
        }
    }
}
