use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// A node's role in the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    #[serde(rename = "main")]
    MainNode,
    #[serde(rename = "external")]
    ExternalNode,
}

impl Display for NodeRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl NodeRole {
    pub fn is_main(&self) -> bool {
        self == &NodeRole::MainNode
    }

    pub fn is_external(&self) -> bool {
        self == &NodeRole::ExternalNode
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            NodeRole::MainNode => "main",
            NodeRole::ExternalNode => "external",
        }
    }
}
