/// A node's role in the network.
#[derive(Debug, PartialEq, Eq)]
pub enum NodeRole {
    MainNode,
    ExternalNode,
}

impl NodeRole {
    pub fn is_main(&self) -> bool {
        self == &NodeRole::MainNode
    }

    pub fn is_external(&self) -> bool {
        self == &NodeRole::ExternalNode
    }
}
