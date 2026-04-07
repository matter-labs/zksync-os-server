use std::sync::atomic::{AtomicUsize, Ordering};
use zksync_os_types::NodeRole;

static NODE_LOG_COUNTERS: NodeLogCounters = NodeLogCounters::new();

pub(crate) struct NodeLogCounters {
    next_main_node_id: AtomicUsize,
    next_external_node_id: AtomicUsize,
}

impl NodeLogCounters {
    pub(crate) const fn new() -> Self {
        Self {
            next_main_node_id: AtomicUsize::new(1),
            next_external_node_id: AtomicUsize::new(1),
        }
    }

    pub(crate) fn next_base_tag(&self, role: NodeRole) -> String {
        let (prefix, counter) = match role {
            NodeRole::MainNode => ("mn", &self.next_main_node_id),
            NodeRole::ExternalNode => ("en", &self.next_external_node_id),
        };
        let id = counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{id}")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NodeLogState {
    base_tag: String,
    restart_count: usize,
}

impl NodeLogState {
    pub(crate) fn fresh(role: NodeRole) -> Self {
        Self {
            base_tag: NODE_LOG_COUNTERS.next_base_tag(role),
            restart_count: 0,
        }
    }

    pub(crate) fn restarted(mut self) -> Self {
        self.restart_count += 1;
        self
    }

    pub(crate) fn tag(&self) -> String {
        if self.restart_count == 0 {
            self.base_tag.clone()
        } else {
            format!("{}-restarted-{}", self.base_tag, self.restart_count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeLogCounters, NodeLogState};
    use zksync_os_types::NodeRole;

    #[test]
    fn node_log_counters_are_separate_per_role() {
        let counters = NodeLogCounters::new();

        assert_eq!(counters.next_base_tag(NodeRole::MainNode), "mn-1");
        assert_eq!(counters.next_base_tag(NodeRole::ExternalNode), "en-1");
        assert_eq!(counters.next_base_tag(NodeRole::MainNode), "mn-2");
        assert_eq!(counters.next_base_tag(NodeRole::ExternalNode), "en-2");
    }

    #[test]
    fn restart_tag_reuses_base_tag_and_increments_suffix() {
        let base = NodeLogState {
            base_tag: "mn-1".to_owned(),
            restart_count: 0,
        };

        assert_eq!(base.tag(), "mn-1");
        assert_eq!(base.clone().restarted().tag(), "mn-1-restarted-1");
        assert_eq!(base.restarted().restarted().tag(), "mn-1-restarted-2");
    }
}
