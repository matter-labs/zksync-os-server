use alloy::primitives::{Address, B256, BlockNumber};
use reth_network_peers::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

/// Tracked protocol session state for a currently connected peer.
#[derive(Debug, Clone)]
pub struct PeerSession {
    pub identity: PeerIdentity,
    pub connected_at: Instant,
    pub replay: Option<ReplaySession>,
    pub verifier: Option<VerifierSession>,
}

/// Stable peer identity information captured when the connection is established.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    pub peer_id: PeerId,
    pub remote_addr: SocketAddr,
}

/// Replay-stream progress tracked for one peer session.
#[derive(Debug, Clone)]
pub struct ReplaySession {
    pub requested_from_block: BlockNumber,
    pub last_block_sent: Option<BlockNumber>,
    pub last_block_sent_at: Option<Instant>,
}

impl ReplaySession {
    pub fn can_verify(&self, required_block: BlockNumber) -> bool {
        matches!(self.last_block_sent, Some(sent) if sent >= required_block)
    }
}

/// Verifier-related state tracked for one peer session.
#[derive(Debug, Clone)]
pub struct VerifierSession {
    pub auth_state: VerifierAuthState,
    pub last_verified_batch: Option<u64>,
    pub last_verified_at: Option<Instant>,
}

/// Current verifier authentication state for a peer session.
#[derive(Debug, Clone)]
pub enum VerifierAuthState {
    RoleRequested,
    Challenged { nonce: B256 },
    Authorized { signer: Address },
    Unauthorized { signer: Option<Address> },
}

/// In-memory store of live peer sessions derived from protocol events.
#[derive(Debug, Default)]
pub struct PeerSessionStore {
    sessions: HashMap<PeerId, PeerSession>,
}

impl PeerSessionStore {
    /// Inserts or replaces the live session tracked for `peer_id`.
    pub fn insert(&mut self, connected_at: Instant, peer_id: PeerId, remote_addr: SocketAddr) {
        self.sessions.insert(
            peer_id,
            PeerSession {
                identity: PeerIdentity {
                    peer_id,
                    remote_addr,
                },
                connected_at,
                replay: None,
                verifier: None,
            },
        );
    }

    /// Removes the tracked session for `peer_id`, returning the last known state if present.
    pub fn remove(&mut self, peer_id: PeerId) -> Option<PeerSession> {
        self.sessions.remove(&peer_id)
    }

    /// Records that `peer_id` requested replay streaming starting from `starting_block`.
    pub fn replay_requested(&mut self, peer_id: PeerId, starting_block: BlockNumber) {
        if let Some(session) = self.sessions.get_mut(&peer_id) {
            session.replay = Some(ReplaySession {
                requested_from_block: starting_block,
                last_block_sent: None,
                last_block_sent_at: None,
            });
        }
    }

    /// Advances replay progress for `peer_id` after block `block_number` was sent successfully.
    pub fn replay_block_sent(&mut self, now: Instant, peer_id: PeerId, block_number: BlockNumber) {
        if let Some(replay) = self
            .sessions
            .get_mut(&peer_id)
            .and_then(|session| session.replay.as_mut())
        {
            replay.last_block_sent = Some(block_number);
            replay.last_block_sent_at = Some(now);
        }
    }

    /// Records that `peer_id` asked to act as a verifier for the current connection.
    pub fn verifier_role_requested(&mut self, peer_id: PeerId) {
        if let Some(session) = self.sessions.get_mut(&peer_id) {
            session.verifier = Some(VerifierSession {
                auth_state: VerifierAuthState::RoleRequested,
                last_verified_batch: None,
                last_verified_at: None,
            });
        }
    }

    /// Records that the main node challenged `peer_id` with `nonce`.
    pub fn verifier_challenged(&mut self, peer_id: PeerId, nonce: B256) {
        if let Some(session) = self.sessions.get_mut(&peer_id) {
            session.verifier = Some(VerifierSession {
                auth_state: VerifierAuthState::Challenged { nonce },
                last_verified_batch: None,
                last_verified_at: None,
            });
        }
    }

    /// Records that `peer_id` proved control of an accepted verifier signer.
    pub fn verifier_authorized(&mut self, peer_id: PeerId, signer: Address) {
        if let Some(session) = self.sessions.get_mut(&peer_id) {
            session.verifier = Some(VerifierSession {
                auth_state: VerifierAuthState::Authorized { signer },
                last_verified_batch: None,
                last_verified_at: None,
            });
        }
    }

    /// Records that `peer_id` failed verifier authorization.
    pub fn verifier_unauthorized(&mut self, peer_id: PeerId, signer: Option<Address>) {
        if let Some(session) = self.sessions.get_mut(&peer_id) {
            session.verifier = Some(VerifierSession {
                auth_state: VerifierAuthState::Unauthorized { signer },
                last_verified_batch: None,
                last_verified_at: None,
            });
        }
    }

    /// Returns the currently tracked session for `peer_id`, if any.
    pub fn get(&self, peer_id: PeerId) -> Option<&PeerSession> {
        self.sessions.get(&peer_id)
    }

    /// Returns peer IDs that are currently eligible to receive batch-verification requests.
    ///
    /// A peer is eligible only if it has both:
    /// - replayed at least through `required_block`
    /// - completed verifier authentication successfully for the current session
    ///
    /// Note: replay eligibility here is based only on blocks the main node has sent, not on any
    /// acknowledgment that the peer actually consumed, persisted, or applied those replay records.
    pub fn authorized_verifier_peers(
        &self,
        required_block: BlockNumber,
    ) -> impl Iterator<Item = PeerId> + '_ {
        self.sessions.values().filter_map(move |session| {
            let replay = session.replay.as_ref()?;
            let verifier = session.verifier.as_ref()?;
            if !replay.can_verify(required_block) {
                return None;
            }
            match verifier.auth_state {
                VerifierAuthState::Authorized { .. } => Some(session.identity.peer_id),
                _ => None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{PeerSessionStore, VerifierAuthState};
    use alloy::primitives::{Address, b512};
    use reth_network_peers::PeerId;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    fn peer_id() -> PeerId {
        b512!(
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"
        )
    }

    fn socket_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn signer(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    #[test]
    fn replay_progress_updates_verification_eligibility() {
        let mut store = PeerSessionStore::default();
        let now = Instant::now();
        let peer_id = peer_id();
        store.insert(now, peer_id, socket_addr(30303));

        let session = store.get(peer_id).unwrap();
        assert!(session.replay.is_none());

        store.replay_requested(peer_id, 7);
        store.replay_block_sent(now + Duration::from_secs(1), peer_id, 9);
        let session = store.get(peer_id).unwrap();
        let replay = session.replay.as_ref().unwrap();
        assert_eq!(replay.requested_from_block, 7);
        assert_eq!(replay.last_block_sent, Some(9));
        assert!(!replay.can_verify(10));

        store.replay_block_sent(now + Duration::from_secs(2), peer_id, 10);
        let session = store.get(peer_id).unwrap();
        assert!(session.replay.as_ref().unwrap().can_verify(10));
    }

    #[test]
    fn removing_session_drops_it_from_store() {
        let mut store = PeerSessionStore::default();
        let peer_id = peer_id();
        store.insert(Instant::now(), peer_id, socket_addr(30304));

        let removed = store.remove(peer_id);
        assert!(removed.is_some());
        assert!(store.get(peer_id).is_none());
    }

    #[test]
    fn verifier_auth_state_transitions_are_tracked() {
        let mut store = PeerSessionStore::default();
        let peer_id = peer_id();
        store.insert(Instant::now(), peer_id, socket_addr(30305));

        store.verifier_role_requested(peer_id);
        let session = store.get(peer_id).unwrap();
        assert!(matches!(
            session.verifier.as_ref().unwrap().auth_state,
            VerifierAuthState::RoleRequested
        ));

        let nonce = alloy::primitives::B256::repeat_byte(0xAB);
        store.verifier_challenged(peer_id, nonce);
        let session = store.get(peer_id).unwrap();
        assert!(matches!(
            session.verifier.as_ref().unwrap().auth_state,
            VerifierAuthState::Challenged { nonce: observed } if observed == nonce
        ));

        let authorized_signer = signer(0x11);
        store.verifier_authorized(peer_id, authorized_signer);
        let session = store.get(peer_id).unwrap();
        assert!(matches!(
            session.verifier.as_ref().unwrap().auth_state,
            VerifierAuthState::Authorized { signer } if signer == authorized_signer
        ));

        let unauthorized_signer = signer(0x22);
        store.verifier_unauthorized(peer_id, Some(unauthorized_signer));
        let session = store.get(peer_id).unwrap();
        assert!(matches!(
            session.verifier.as_ref().unwrap().auth_state,
            VerifierAuthState::Unauthorized { signer } if signer == Some(unauthorized_signer)
        ));
    }

    #[test]
    fn authorized_verifier_peers_require_auth_and_replay_progress() {
        let mut store = PeerSessionStore::default();
        let now = Instant::now();

        let eligible_peer = peer_id();
        let lagging_peer = b512!(
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002"
        );
        let unauthorized_peer = b512!(
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003"
        );

        store.insert(now, eligible_peer, socket_addr(30306));
        store.insert(now, lagging_peer, socket_addr(30307));
        store.insert(now, unauthorized_peer, socket_addr(30308));

        for peer in [eligible_peer, lagging_peer, unauthorized_peer] {
            store.replay_requested(peer, 7);
        }

        store.replay_block_sent(now + Duration::from_secs(1), eligible_peer, 10);
        store.replay_block_sent(now + Duration::from_secs(1), lagging_peer, 9);
        store.replay_block_sent(now + Duration::from_secs(1), unauthorized_peer, 10);

        store.verifier_authorized(eligible_peer, signer(0x11));
        store.verifier_authorized(lagging_peer, signer(0x22));
        store.verifier_unauthorized(unauthorized_peer, Some(signer(0x33)));

        let peers: Vec<_> = store.authorized_verifier_peers(10).collect();
        assert_eq!(peers, vec![eligible_peer]);
    }
}
