use alloy::primitives::{Address, B256, BlockNumber, Bytes};
use alloy::signers::local::PrivateKeySigner;
use assert_matches::assert_matches;
use futures::StreamExt;
use reth_network::events::PeerEvent;
use reth_network::test_utils::Peer;
use reth_network::{NetworkEvent, Peers, PeersInfo, test_utils::Testnet};
use reth_network_peers::PeerId;
use reth_provider::test_utils::MockEthProvider;
use reth_provider::{BlockReader, HeaderProvider};
use secrecy::{ExposeSecret, SecretString};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_network::protocol::{
    ExternalNodeProtocolConfig, HandlerSharedState, ProtocolEvent, ZksProtocolHandler,
};
use zksync_os_network::twofa::{ExternalNode2faConfig, MainNode2faConfig, Zks2faProtocolHandler};
use zksync_os_network::version::{ZksProtocolV0, ZksProtocolV5, ZksProtocolVersionSpec};
use zksync_os_network::{
    PeerVerifyBatchResult, RecordOverride, VerifyBatchOutcome, VerifyBatchResult,
};
use zksync_os_storage_api::BlockContext;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::{BlockStartCursors, NodeRole, ProtocolSemanticVersion};

#[derive(Debug, Clone, Default)]
struct InMemReplay {
    canonical: HashMap<BlockNumber, ReplayRecord>,
    /// Rows stored under explicit db keys, mirroring how reverted records are kept in RocksDB.
    by_key: HashMap<Vec<u8>, ReplayRecord>,
}

impl InMemReplay {
    fn new(replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>) -> Self {
        Self {
            canonical: HashMap::from_iter(replays),
            by_key: HashMap::new(),
        }
    }

    fn with_override(mut self, db_key: Vec<u8>, record: ReplayRecord) -> Self {
        self.by_key.insert(db_key, record);
        self
    }
}

impl ReadReplay for InMemReplay {
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
        self.canonical.get(&block_number).map(|r| r.block_context)
    }

    fn get_replay_record_by_key(
        &self,
        block_number: BlockNumber,
        db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        match db_key {
            Some(db_key) => self.by_key.get(&db_key).cloned(),
            None => self.canonical.get(&block_number).cloned(),
        }
    }

    fn latest_record(&self) -> BlockNumber {
        self.canonical.keys().last().copied().unwrap_or_default()
    }
}

fn dummy_record<P: ZksProtocolVersionSpec>(block_number: BlockNumber) -> ReplayRecord {
    // Do full round conversion ReplayRecord->P::Record->ReplayRecord to get rid of unsupported
    // fields for each protocol version (e.g. everything except the block number is dropped for
    // the test-only v0).
    let record = ReplayRecord::new(
        BlockContext {
            block_number,
            ..Default::default()
        },
        vec![],
        24,
        // Important that this is set to `NODE_SEMVER_VERSION` as the wire formats do not transport
        // node version over the network. Instead, receiver stamps all records with its current
        // node version.
        NODE_SEMVER_VERSION.clone(),
        ProtocolSemanticVersion::new(4, 5, 6),
        B256::random(),
        vec![],
        BlockStartCursors {
            l1_priority_id: 42,
            interop_root_id: 0,
            migration_number: 123,
            interop_fee_number: 456,
        },
    );
    let zks_record: P::Record = record.into();
    zks_record
        .try_into()
        .expect("failed to do full round conversion")
}

fn default_verifier_signing_key() -> SecretString {
    SecretString::from("0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110")
}

fn alternate_verifier_signing_key() -> SecretString {
    SecretString::from("0x59c6995e998f97a5a0044966f094538e5f7d918e2f8b3bf3f1e9465d9b38787e")
}

fn accepted_verifier_signers() -> Vec<Address> {
    vec![
        PrivateKeySigner::from_str(default_verifier_signing_key().expose_secret())
            .unwrap()
            .address(),
    ]
}

fn dummy_verify_batch_result() -> VerifyBatchResult {
    VerifyBatchResult {
        request_id: 41,
        batch_number: 7,
        result: VerifyBatchOutcome::Approved(Bytes::from(vec![9u8; 65])),
    }
}

struct TestPeerProtocolHandles {
    protocol_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
    replay_rx: mpsc::Receiver<ReplayRecord>,
    verify_result_rx: Option<mpsc::Receiver<PeerVerifyBatchResult>>,
    outgoing_verify_results_tx: Option<broadcast::Sender<PeerVerifyBatchResult>>,
}

trait PeerExt {
    fn add_zks_sub_protocol<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
    ) -> (
        mpsc::UnboundedReceiver<ProtocolEvent>,
        mpsc::Receiver<ReplayRecord>,
    );

    fn add_zks_sub_protocol_with_test_handles<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
        trusted_peers: HashSet<PeerId>,
    ) -> TestPeerProtocolHandles;

    fn add_zks_sub_protocol_with_storage<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: InMemReplay,
        record_overrides: Vec<RecordOverride>,
        max_active_connections: usize,
        trusted_peers: HashSet<PeerId>,
    ) -> TestPeerProtocolHandles;

    /// Registers `zks/5` replay and `zks_2fa` verification on the same peer.
    ///
    /// Both handlers publish into one `ProtocolEvent` stream, matching production. Only an
    /// external-node peer needs `verifier_signing_key`.
    fn add_zks_2fa_sub_protocol(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
        verifier_signing_key: Option<SecretString>,
    ) -> TestPeerProtocolHandles;
}

impl<C> PeerExt for Peer<C>
where
    C: BlockReader + HeaderProvider + Clone + 'static,
{
    fn add_zks_sub_protocol<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
    ) -> (
        mpsc::UnboundedReceiver<ProtocolEvent>,
        mpsc::Receiver<ReplayRecord>,
    ) {
        let TestPeerProtocolHandles {
            protocol_rx,
            replay_rx,
            ..
        } = self.add_zks_sub_protocol_with_test_handles::<P>(
            node_role,
            starting_block,
            replays,
            max_active_connections,
            HashSet::new(),
        );
        (protocol_rx, replay_rx)
    }

    fn add_zks_sub_protocol_with_test_handles<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
        trusted_peers: HashSet<PeerId>,
    ) -> TestPeerProtocolHandles {
        self.add_zks_sub_protocol_with_storage::<P>(
            node_role,
            starting_block,
            InMemReplay::new(replays),
            vec![],
            max_active_connections,
            trusted_peers,
        )
    }

    fn add_zks_sub_protocol_with_storage<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: InMemReplay,
        record_overrides: Vec<RecordOverride>,
        max_active_connections: usize,
        trusted_peers: HashSet<PeerId>,
    ) -> TestPeerProtocolHandles {
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let (replay_tx, replay_rx) = mpsc::channel(8);
        let state = HandlerSharedState::new(protocol_tx, max_active_connections, trusted_peers);
        let handler = if node_role.is_main() {
            ZksProtocolHandler::<P, _>::for_main_node(replays, state)
        } else {
            ZksProtocolHandler::<P, _>::for_external_node(
                replays,
                ExternalNodeProtocolConfig {
                    starting_block: Arc::new(RwLock::new(starting_block)),
                    record_overrides,
                    max_blocks_per_message: 64,
                    replay_sender: replay_tx,
                    verification: None,
                    // Generous enough that the inactivity timeout never affects these tests.
                    replay_inactivity_timeout: std::time::Duration::from_secs(600),
                },
                state,
            )
        };
        self.add_rlpx_sub_protocol(handler);
        TestPeerProtocolHandles {
            protocol_rx,
            replay_rx,
            verify_result_rx: None,
            outgoing_verify_results_tx: None,
        }
    }

    fn add_zks_2fa_sub_protocol(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
        verifier_signing_key: Option<SecretString>,
    ) -> TestPeerProtocolHandles {
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let (replay_tx, replay_rx) = mpsc::channel(8);
        // Both subprotocols share one event stream, exactly as in production.
        let zks_state =
            HandlerSharedState::new(protocol_tx.clone(), max_active_connections, HashSet::new());
        let twofa_state =
            HandlerSharedState::new(protocol_tx, max_active_connections, HashSet::new());
        let zks_2fa_registry = Arc::new(RwLock::new(HashMap::new()));
        let replays = InMemReplay::new(replays);

        let (verify_result_rx, outgoing_verify_results_tx) = if node_role.is_main() {
            let (verify_result_tx, verify_result_rx) = mpsc::channel(8);
            self.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV5, _>::for_main_node(
                replays, zks_state,
            ));
            self.add_rlpx_sub_protocol(Zks2faProtocolHandler::for_main_node(
                MainNode2faConfig {
                    accepted_verifier_signers: accepted_verifier_signers(),
                    verify_result_tx,
                },
                twofa_state,
                zks_2fa_registry,
            ));
            (Some(verify_result_rx), None)
        } else {
            let signing_key =
                verifier_signing_key.expect("external verifier node requires a signing key");
            let (verify_batch_tx, _verify_batch_rx) = mpsc::channel(8);
            let (outgoing_verify_results, _outgoing_verify_results_rx) = broadcast::channel(8);
            self.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV5, _>::for_external_node(
                replays,
                ExternalNodeProtocolConfig {
                    starting_block: Arc::new(RwLock::new(starting_block)),
                    record_overrides: vec![],
                    max_blocks_per_message: 64,
                    replay_sender: replay_tx,
                    verification: None,
                    // Generous enough that the inactivity timeout never affects these tests.
                    replay_inactivity_timeout: std::time::Duration::from_secs(600),
                },
                zks_state,
            ));
            self.add_rlpx_sub_protocol(Zks2faProtocolHandler::for_external_node(
                ExternalNode2faConfig {
                    signing_key,
                    verify_batch_tx,
                    outgoing_verify_results: outgoing_verify_results.clone(),
                },
                twofa_state,
                zks_2fa_registry,
            ));
            (None, Some(outgoing_verify_results))
        };

        TestPeerProtocolHandles {
            protocol_rx,
            replay_rx,
            verify_result_rx,
            outgoing_verify_results_tx,
        }
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_matching_version() {
    // Run two peers that both communicate on exactly one matching zks protocol and successfully
    // transfer one replay record from peer0 to peer1.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
    );
    let (mut from_peer1, mut replay_rx_peer1) = net.peers_mut()[1]
        .add_zks_sub_protocol::<ZksProtocolV5>(
            NodeRole::ExternalNode,
            1,
            [(1, record1.clone())],
            100,
        );

    let handle = net.spawn();
    handle.connect_peers().await;

    assert_matches!(from_peer0.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, *handle.peers()[1].peer_id());
    });
    assert_matches!(from_peer1.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, *handle.peers()[0].peer_id());
    });

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn emits_replay_session_events() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_established = false;
    let mut saw_replay_requested = false;
    let mut saw_replay_block_sent = false;

    while !(saw_established && saw_replay_requested && saw_replay_block_sent) {
        match from_peer0.recv().await {
            Some(ProtocolEvent::Established { peer_id, .. }) => {
                assert_eq!(peer_id, peer1_id);
                saw_established = true;
            }
            Some(ProtocolEvent::ReplayRequested {
                peer_id,
                starting_block,
            }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(starting_block, 1);
                saw_replay_requested = true;
            }
            Some(ProtocolEvent::ReplayBlockSent {
                peer_id,
                block_number,
            }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(block_number, 1);
                saw_replay_block_sent = true;
            }
            Some(ProtocolEvent::VerifierRoleRequested { .. }) => {
                panic!("unexpected verifier role request event")
            }
            Some(ProtocolEvent::Closed { .. }) => {}
            Some(
                ProtocolEvent::VerifierChallengeSent { .. }
                | ProtocolEvent::VerifierAuthorized { .. }
                | ProtocolEvent::VerifierUnauthorized { .. },
            ) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            Some(ProtocolEvent::ReplayStreamStalled { .. }) => {
                panic!("unexpected replay stream stall event")
            }
            None => panic!("protocol event stream closed before replay events were observed"),
        }
    }

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn serves_overridden_replay_records() {
    // Regression test for the reverted-block debugging flow (#657): an EN that requests record
    // overrides must be served the rows stored under the overridden db keys instead of the
    // canonical ones.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);
    let canonical2 = dummy_record::<ZksProtocolV5>(2);
    let reverted2 = dummy_record::<ZksProtocolV5>(2);
    assert_ne!(canonical2, reverted2);
    // Opaque to the protocol; in production this is the reverted block's hash.
    let db_key = vec![0xAB; 32];

    net.peers_mut()[0].add_zks_sub_protocol_with_storage::<ZksProtocolV5>(
        NodeRole::MainNode,
        0,
        InMemReplay::new([(1, record1.clone()), (2, canonical2.clone())])
            .with_override(db_key.clone(), reverted2.clone()),
        vec![],
        100,
        HashSet::new(),
    );
    let mut external = net.peers_mut()[1].add_zks_sub_protocol_with_storage::<ZksProtocolV5>(
        NodeRole::ExternalNode,
        1,
        InMemReplay::new([(1, record1.clone()), (2, canonical2.clone())]),
        vec![RecordOverride {
            block_number: 2,
            db_key: db_key.into(),
        }],
        100,
        HashSet::new(),
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    assert_eq!(external.replay_rx.recv().await.unwrap(), record1);
    assert_eq!(external.replay_rx.recv().await.unwrap(), reverted2);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn batches_multiple_replay_records() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);
    let record2 = dummy_record::<ZksProtocolV5>(2);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone()), (2, record2.clone())],
        100,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone()), (2, record2.clone())],
        100,
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut replay_blocks_sent = Vec::new();
    while replay_blocks_sent.len() < 2 {
        match from_peer0.recv().await {
            Some(ProtocolEvent::ReplayBlockSent {
                peer_id,
                block_number,
            }) => {
                assert_eq!(peer_id, peer1_id);
                replay_blocks_sent.push(block_number);
            }
            Some(
                ProtocolEvent::Established { .. }
                | ProtocolEvent::ReplayRequested { .. }
                | ProtocolEvent::Closed { .. },
            ) => {}
            Some(
                ProtocolEvent::VerifierRoleRequested { .. }
                | ProtocolEvent::VerifierChallengeSent { .. }
                | ProtocolEvent::VerifierAuthorized { .. }
                | ProtocolEvent::VerifierUnauthorized { .. },
            ) => panic!("unexpected verifier event during replay batching test"),
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            Some(ProtocolEvent::ReplayStreamStalled { .. }) => {
                panic!("unexpected replay stream stall event")
            }
            None => {
                panic!("protocol event stream closed before batched replay events were observed")
            }
        }
    }

    assert_eq!(replay_blocks_sent, vec![1, 2]);
    assert_eq!(replay_rx_peer1.recv().await.unwrap(), record1);
    assert_eq!(replay_rx_peer1.recv().await.unwrap(), record2);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_different_versions() {
    // Peer 0 advertises `zks/0` and `zks/5`, while peer 1 advertises only `zks/0`. They must select
    // `zks/0`, whose replay record preserves only the block number.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);
    let (_, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
    );
    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV0>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
    );

    let (mut from_peer1, mut replay_rx_peer1) = net.peers_mut()[1]
        .add_zks_sub_protocol::<ZksProtocolV0>(
            NodeRole::ExternalNode,
            1,
            [(1, record1.clone())],
            100,
        );

    let handle = net.spawn();
    handle.connect_peers().await;

    assert_matches!(from_peer0.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, *handle.peers()[1].peer_id());
    });
    assert_matches!(from_peer1.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, *handle.peers()[0].peer_id());
    });

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    // The negotiated v0 format deliberately discards every other field.
    assert_ne!(received_replay_record, record1);
    assert_eq!(
        received_replay_record.block_context.block_number,
        record1.block_context.block_number
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn disconnects_peer_without_common_zks_version() {
    // Peer 0 speaks only `zks/5` and peer 1 only `zks/0`, so capability negotiation yields no
    // shared `zks` version. The handler must drop the whole session during capability negotiation
    // of letting a never-syncing peer occupy a connection slot.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;

    let (mut from_peer0, _) =
        net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(NodeRole::MainNode, 0, [], 100);
    let peer1 = &mut net.peers_mut()[1];
    let peer1_id = peer1.peer_id();
    let peer1_addr = peer1.local_addr();
    let (mut from_peer1, _) =
        peer1.add_zks_sub_protocol::<ZksProtocolV0>(NodeRole::ExternalNode, 1, [], 100);

    let handle = net.spawn();
    // `connect_peers()` would hang here: it waits for sessions that never establish. Dial
    // manually instead.
    let mut peer0_events = handle.peers()[0].event_listener();
    handle.peers()[0].network().add_peer(peer1_id, peer1_addr);

    // Confirm the dial was registered so the negative assertions below cannot pass vacuously.
    loop {
        match peer0_events.next().await {
            Some(NetworkEvent::Peer(PeerEvent::PeerAdded(added))) => {
                assert_eq!(added, peer1_id);
                break;
            }
            Some(_) => {}
            None => panic!("network event stream closed before the peer was added"),
        }
    }

    // Rejection happens before either handler can emit `ProtocolEvent::Established` and before reth
    // counts the peer as connected.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), from_peer0.recv())
            .await
            .is_err(),
        "peer0 must not establish a zks connection with a version-disjoint peer"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), from_peer1.recv())
            .await
            .is_err(),
        "peer1 must not establish a zks connection with a version-disjoint peer"
    );
    assert_eq!(handle.peers()[0].network().num_connected_peers(), 0);
    assert_eq!(handle.peers()[1].network().num_connected_peers(), 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn max_active_connections() {
    // Run three peers while peer0 has max active connections set to 1. peer1 is expected to be
    // successfully connected first while peer2 is expected to error out with
    // `MaxActiveConnectionsExceeded`.
    let mut net = Testnet::create_with(3, MockEthProvider::default()).await;

    let (mut from_peer0, _) =
        net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV5>(NodeRole::MainNode, 1, [], 1);

    let peer1 = &mut net.peers_mut()[1];
    let peer1_id = peer1.peer_id();
    let peer1_addr = peer1.local_addr();
    let (_, _) = peer1.add_zks_sub_protocol::<ZksProtocolV5>(NodeRole::ExternalNode, 1, [], 100);

    let peer2 = &mut net.peers_mut()[2];
    let peer2_id = peer2.peer_id();
    let peer2_addr = peer2.local_addr();
    let (_, _) = peer2.add_zks_sub_protocol::<ZksProtocolV5>(NodeRole::ExternalNode, 1, [], 100);

    let handle = net.spawn();

    // Connect peers 0 and 1
    let peer0_handle = &handle.peers()[0];
    peer0_handle.network().add_peer(peer1_id, peer1_addr);
    assert_matches!(from_peer0.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, *peer1_id);
    });

    // Connect peers 0 and 2, max active connections exceeded
    peer0_handle.network().add_peer(peer2_id, peer2_addr);
    loop {
        match from_peer0.recv().await {
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { max_connections }) => {
                assert_eq!(max_connections, 1);
                break;
            }
            Some(
                ProtocolEvent::ReplayRequested { .. }
                | ProtocolEvent::ReplayBlockSent { .. }
                | ProtocolEvent::VerifierRoleRequested { .. }
                | ProtocolEvent::VerifierChallengeSent { .. }
                | ProtocolEvent::VerifierAuthorized { .. }
                | ProtocolEvent::VerifierUnauthorized { .. },
            ) => {}
            other => panic!("unexpected protocol event: {other:?}"),
        }
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn trusted_peer_bypasses_max_active_connections() {
    // peer0 (main node) allows zero active connections, so only a trusted peer can connect. Its
    // outgoing dial to trusted peer1 is admitted regardless of the cap.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let peer1_id = net.peers_mut()[1].peer_id();

    let mut from_peer0 = net.peers_mut()[0]
        .add_zks_sub_protocol_with_test_handles::<ZksProtocolV5>(
            NodeRole::MainNode,
            1,
            [],
            0,
            HashSet::from([peer1_id]),
        )
        .protocol_rx;
    let peer1_addr = net.peers_mut()[1].local_addr();
    net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV5>(NodeRole::ExternalNode, 1, [], 100);

    let handle = net.spawn();
    handle.peers()[0].network().add_peer(peer1_id, peer1_addr);

    assert_matches!(from_peer0.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
        assert_eq!(peer_id, peer1_id);
    });
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zks_2fa_authorizes_verifier_and_replays() {
    // A verifier peer uses independent lanes: replay over `zks/5` and authentication over
    // `zks_2fa`. Both must make progress on the same RLPx connection.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);
    let expected_signer = PrivateKeySigner::from_str(
        "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
    )
    .unwrap()
    .address();

    let mut main = net.peers_mut()[0].add_zks_2fa_sub_protocol(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        None,
    );
    let mut external = net.peers_mut()[1].add_zks_2fa_sub_protocol(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        Some(default_verifier_signing_key()),
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_verifier_authorized = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_authorized && saw_replay_requested) {
        match main.protocol_rx.recv().await {
            Some(ProtocolEvent::VerifierAuthorized { peer_id, signer }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(signer, expected_signer);
                saw_verifier_authorized = true;
            }
            Some(ProtocolEvent::ReplayRequested {
                peer_id,
                starting_block,
            }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(starting_block, 1);
                saw_replay_requested = true;
            }
            Some(ProtocolEvent::VerifierUnauthorized { signer, .. }) => {
                panic!("unexpected verifier unauthorized event: {signer:?}")
            }
            Some(
                ProtocolEvent::Established { .. }
                | ProtocolEvent::Closed { .. }
                | ProtocolEvent::ReplayBlockSent { .. }
                | ProtocolEvent::VerifierRoleRequested { .. }
                | ProtocolEvent::VerifierChallengeSent { .. },
            ) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            Some(ProtocolEvent::ReplayStreamStalled { .. }) => {
                panic!("unexpected replay stream stall event")
            }
            None => panic!("event stream closed before verifier auth + replay were observed"),
        }
    }

    let received_replay_record = external.replay_rx.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zks_2fa_emits_verifier_unauthorized() {
    // A verifier EN authenticating with a signer the main node does not accept must be reported
    // as unauthorized over `zks_2fa`, while replay still proceeds over the independent `zks/5`
    // lane.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);

    let mut main = net.peers_mut()[0].add_zks_2fa_sub_protocol(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        None,
    );
    let mut external = net.peers_mut()[1].add_zks_2fa_sub_protocol(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        Some(alternate_verifier_signing_key()),
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_verifier_unauthorized = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_unauthorized && saw_replay_requested) {
        match main.protocol_rx.recv().await {
            Some(ProtocolEvent::VerifierUnauthorized { peer_id, signer }) => {
                assert_eq!(peer_id, peer1_id);
                assert!(signer.is_some());
                saw_verifier_unauthorized = true;
            }
            Some(ProtocolEvent::ReplayRequested {
                peer_id,
                starting_block,
            }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(starting_block, 1);
                saw_replay_requested = true;
            }
            Some(ProtocolEvent::VerifierAuthorized { signer, .. }) => {
                panic!("unexpected verifier authorized event: {signer:?}")
            }
            Some(
                ProtocolEvent::Established { .. }
                | ProtocolEvent::Closed { .. }
                | ProtocolEvent::ReplayBlockSent { .. }
                | ProtocolEvent::VerifierRoleRequested { .. }
                | ProtocolEvent::VerifierChallengeSent { .. },
            ) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            Some(ProtocolEvent::ReplayStreamStalled { .. }) => {
                panic!("unexpected replay stream stall event")
            }
            None => panic!("event stream closed before verifier auth failure was observed"),
        }
    }

    let received_replay_record = external.replay_rx.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn zks_2fa_forwards_verify_batch_result_to_main_node() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV5>(1);

    let mut main = net.peers_mut()[0].add_zks_2fa_sub_protocol(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        None,
    );
    let external = net.peers_mut()[1].add_zks_2fa_sub_protocol(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        Some(default_verifier_signing_key()),
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let main_peer_id = *handle.peers()[0].peer_id();
    let external_peer_id = *handle.peers()[1].peer_id();

    // Wait for authentication to complete before injecting a verification result.
    loop {
        match main.protocol_rx.recv().await {
            Some(ProtocolEvent::VerifierAuthorized { peer_id, .. }) => {
                assert_eq!(peer_id, external_peer_id);
                break;
            }
            Some(ProtocolEvent::VerifierUnauthorized { signer, .. }) => {
                panic!("unexpected verifier unauthorized event: {signer:?}")
            }
            Some(_) => {}
            None => panic!("event stream closed before verifier was authorized"),
        }
    }

    let result = dummy_verify_batch_result();
    external
        .outgoing_verify_results_tx
        .as_ref()
        .unwrap()
        .send(PeerVerifyBatchResult {
            peer_id: main_peer_id,
            message: result.clone(),
        })
        .unwrap();

    let forwarded = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        main.verify_result_rx.as_mut().unwrap().recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(forwarded.peer_id, external_peer_id);
    assert_eq!(forwarded.message, result);
}
