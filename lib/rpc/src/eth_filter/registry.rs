use super::EthFilterError;
use super::pending::PendingTransactionKind;
use alloy::primitives::U128;
use alloy::rpc::types::{Filter, FilterId};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::MissedTickBehavior;

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

/// Manages the set of installed filters: install, advance, uninstall, and stale-filter eviction.
#[derive(Clone)]
pub(crate) struct FilterRegistry {
    filters: Arc<DashMap<FilterId, ActiveFilter>>,
    stale_filter_ttl: Duration,
}

impl FilterRegistry {
    pub(crate) fn new(stale_filter_ttl: Duration) -> Self {
        Self {
            filters: Arc::new(DashMap::new()),
            stale_filter_ttl,
        }
    }

    /// Installs a new filter, recording `latest_block` as the starting point for change polling.
    /// Returns the newly assigned filter ID.
    pub(crate) fn install(&self, kind: FilterKind, latest_block: u64) -> FilterId {
        let id = FilterId::Str(format!("0x{:x}", U128::random()));
        self.filters.insert(
            id.clone(),
            ActiveFilter {
                block: latest_block,
                last_poll_timestamp: Instant::now(),
                kind,
            },
        );
        id
    }

    /// Removes an installed filter. Returns `true` if it existed.
    pub(crate) fn uninstall(&self, id: &FilterId) -> bool {
        self.filters.remove(id).is_some()
    }

    /// Returns the log `Filter` for a filter ID, or an error if it does not exist or is not a log
    /// filter.
    pub(crate) fn get_log_filter(&self, id: &FilterId) -> Result<Filter, EthFilterError> {
        let entry = self
            .filters
            .get(id)
            .ok_or_else(|| EthFilterError::FilterNotFound(id.clone()))?;
        entry
            .kind
            .as_log_filter()
            .cloned()
            .ok_or_else(|| EthFilterError::FilterNotFound(id.clone()))
    }

    /// Advances the filter's block pointer to `latest_block + 1` and returns
    /// `Some((start_block, kind))` — the range to scan and the filter kind.
    /// Returns `None` when there are no new blocks since the last poll.
    pub(crate) fn advance(
        &self,
        id: FilterId,
        latest_block: u64,
    ) -> Result<Option<(u64, FilterKind)>, EthFilterError> {
        let mut entry = self
            .filters
            .get_mut(&id)
            .ok_or(EthFilterError::FilterNotFound(id))?;

        if entry.block > latest_block {
            return Ok(None);
        }

        // Advance the stored block to `latest_block + 1` so the next poll starts from there.
        let mut start_block = latest_block + 1;
        std::mem::swap(&mut entry.block, &mut start_block);
        entry.last_poll_timestamp = Instant::now();

        Ok(Some((start_block, entry.kind.clone())))
    }

    /// Evicts filters that have not been polled within `stale_filter_ttl`.
    pub(crate) fn clear_stale(&self, now: Instant) {
        self.filters.retain(|id, filter| {
            let is_valid = (now - filter.last_poll_timestamp) < self.stale_filter_ttl;
            if !is_valid {
                tracing::trace!(?id, "evicting stale filter");
            }
            is_valid
        });
    }

    /// Runs an endless loop that calls [`Self::clear_stale`] on every `stale_filter_ttl` tick.
    pub(crate) async fn watch_and_clear_stale(&self) {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + self.stale_filter_ttl,
            self.stale_filter_ttl,
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.clear_stale(Instant::now());
        }
    }
}
