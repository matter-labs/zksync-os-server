use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Methods the server rejects with -32601.
/// Populate this with the stateful filter family when running behind a load balancer without sticky sessions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MethodFilter(pub HashSet<String>);

impl MethodFilter {
    pub(crate) fn rejects(&self, method: &str) -> bool {
        self.0.contains(method)
    }
}
