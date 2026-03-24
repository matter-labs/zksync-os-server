use super::pending::PendingTransactionKind;
use alloy::rpc::types::Filter;
use std::time::Instant;

/// An active installed filter
#[derive(Debug)]
pub(crate) struct ActiveFilter {
    /// At which block the filter was polled last.
    pub(crate) block: u64,
    /// Last time this filter was polled.
    pub(crate) last_poll_timestamp: Instant,
    /// What kind of filter it is.
    pub(crate) kind: FilterKind,
}

#[derive(Clone, Debug)]
pub(crate) enum FilterKind {
    Log(Box<Filter>),
    Block,
    PendingTransaction(PendingTransactionKind),
}

impl FilterKind {
    pub(crate) fn as_log_filter(&self) -> Option<&Filter> {
        if let Self::Log(filter) = self {
            Some(filter)
        } else {
            None
        }
    }
}
