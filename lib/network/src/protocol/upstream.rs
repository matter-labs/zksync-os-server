use reth_network_peers::PeerId;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Ensures an external node consumes block replays from at most one upstream at a time.
///
/// The EN consumer loop (`run_en_connection`) feeds a single shared `starting_block`/`replay_sender`
/// and asserts strictly sequential block numbers. Two concurrent consumer loops would interleave
/// and panic. This guard holds a capacity-1 semaphore: the permit lives for the consumer task's
/// lifetime, and `peer_id` records who we currently consume from (used for loop prevention).
#[derive(Debug, Clone)]
pub struct UpstreamGuard {
    lock: Arc<Semaphore>,
    peer_id: Arc<Mutex<Option<PeerId>>>,
}

impl UpstreamGuard {
    pub fn new() -> Self {
        Self {
            lock: Arc::new(Semaphore::new(1)),
            peer_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Try to become the single consumer for `peer_id`. On success records `peer_id` and returns a
    /// permit; the permit must be held for as long as the consumer loop runs. Fast, never waits.
    pub(crate) fn try_acquire(&self, peer_id: PeerId) -> Option<OwnedSemaphorePermit> {
        let permit = self.lock.clone().try_acquire_owned().ok()?;
        *self
            .peer_id
            .lock()
            .expect("upstream guard peer_id lock poisoned") = Some(peer_id);
        Some(permit)
    }

    /// The peer we are currently consuming from, if any.
    pub(crate) fn peer_id(&self) -> Option<PeerId> {
        *self
            .peer_id
            .lock()
            .expect("upstream guard peer_id lock poisoned")
    }

    /// Clears the recorded upstream peer. Call when the consumer loop ends (right before the permit
    /// is dropped) so a future link can be selected.
    pub(crate) fn clear(&self) {
        *self
            .peer_id
            .lock()
            .expect("upstream guard peer_id lock poisoned") = None;
    }
}

impl Default for UpstreamGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::UpstreamGuard;
    use alloy::primitives::B512;
    use reth_network_peers::PeerId;

    fn peer_id(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    #[test]
    fn fresh_guard_has_no_upstream_and_first_acquire_wins() {
        let guard = UpstreamGuard::new();
        assert_eq!(guard.peer_id(), None);

        let permit = guard.try_acquire(peer_id(0x11));
        assert!(permit.is_some());
        assert_eq!(guard.peer_id(), Some(peer_id(0x11)));
    }

    #[test]
    fn second_acquire_fails_while_permit_is_held() {
        let guard = UpstreamGuard::new();
        let _permit = guard
            .try_acquire(peer_id(0x11))
            .expect("first acquire wins");

        assert!(guard.try_acquire(peer_id(0x22)).is_none());
        assert_eq!(guard.peer_id(), Some(peer_id(0x11)));
    }

    #[test]
    fn slot_is_reusable_after_drop_and_clear() {
        let guard = UpstreamGuard::new();
        let permit = guard
            .try_acquire(peer_id(0x11))
            .expect("first acquire wins");

        guard.clear();
        drop(permit);
        assert_eq!(guard.peer_id(), None);

        let permit = guard.try_acquire(peer_id(0x22));
        assert!(permit.is_some());
        assert_eq!(guard.peer_id(), Some(peer_id(0x22)));
    }
}
