use std::collections::HashMap;
use std::num::NonZeroU32;

/// Returns the global and per-method RPS limits. Different impls build them from different sources — a config list, tier numbers, etc.
pub trait Policy: Send + Sync {
    fn global(&self) -> Option<NonZeroU32>;
    fn methods(&self) -> HashMap<String, NonZeroU32>;
}

/// Simplest [`Policy`]: each method's limit is supplied manually.
pub struct PerMethod {
    global: Option<NonZeroU32>,
    per_method: HashMap<String, NonZeroU32>,
}

impl PerMethod {
    pub fn new(global: Option<NonZeroU32>, per_method: HashMap<String, NonZeroU32>) -> Self {
        Self { global, per_method }
    }
}

impl Policy for PerMethod {
    fn global(&self) -> Option<NonZeroU32> {
        self.global
    }

    fn methods(&self) -> HashMap<String, NonZeroU32> {
        self.per_method.clone()
    }
}
