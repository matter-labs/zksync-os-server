use alloy::primitives::{Address, B256, BlockNumber, Bytes};
use alloy::signers::local::PrivateKeySigner;
use assert_matches::assert_matches;
use reth_network::test_utils::Peer;
use reth_network::{Peers, test_utils::Testnet};
use reth_provider::test_utils::MockEthProvider;
use reth_provider::{BlockReader, HeaderProvider};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use test_casing::test_casing;
use tokio::sync::{broadcast, mpsc};
use zksync_os_interface::types::BlockContext;
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_network::protocol::{
    ExternalNodeProtocolConfig, ExternalNodeVerifierConfig, HandlerSharedState,
    MainNodeProtocolConfig, ProtocolEvent, ZksProtocolHandler,
};
use zksync_os_network::version::{
    ZksProtocolV0, ZksProtocolV1, ZksProtocolV2, ZksProtocolV3, ZksProtocolVersionSpec, ZksVersion,
};
use zksync_os_network::{PeerVerifyBatchResult, VerifyBatchOutcome, VerifyBatchResult};
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::{BlockStartCursors, NodeRole, ProtocolSemanticVersion};

#[derive(Debug, Clone, Default)]
struct InMemReplay(HashMap<BlockNumber, ReplayRecord>);

impl ReadReplay for InMemReplay {
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
        self.0.get(&block_number).map(|r| r.block_context)
    }

    fn get_replay_record_by_key(
        &self,
        block_number: BlockNumber,
        _db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        self.0.get(&block_number).cloned()
    }

    fn latest_record(&self) -> BlockNumber {
        self.0.keys().last().copied().unwrap_or_default()
    }
}

fn dummy_record<P: ZksProtocolVersionSpec>(block_number: BlockNumber) -> ReplayRecord {
    // Do full round conversion ReplayRecord->P::Record->ReplayRecord to get rid of unsupported
    // fields for each protocol version (e.g. `starting_migration_number` and
    // `starting_interop_fee_number` will be zeroed out for v1).
    let record = ReplayRecord::new(
        BlockContext {
            block_number,
            ..Default::default()
        },
        vec![],
        24,
        // Important that this is set to `NODE_SEMVER_VERSION` as v1 does not transport node version
        // over the network. Instead, receiver stamps all records with its current node version.
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
        verifier_enabled: bool,
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
        verifier_enabled: bool,
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
            verifier_enabled.then(default_verifier_signing_key),
        );
        (protocol_rx, replay_rx)
    }

    fn add_zks_sub_protocol_with_test_handles<P: ZksProtocolVersionSpec>(
        &mut self,
        node_role: NodeRole,
        starting_block: BlockNumber,
        replays: impl IntoIterator<Item = (BlockNumber, ReplayRecord)>,
        max_active_connections: usize,
        verifier_signing_key: Option<SecretString>,
    ) -> TestPeerProtocolHandles {
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let (replay_tx, replay_rx) = mpsc::channel(8);
        let (verification, outgoing_verify_results_tx) =
            if let Some(signing_key) = verifier_signing_key {
                let (verify_batch_tx, _verify_batch_rx) = mpsc::channel(8);
                let (outgoing_verify_results, _outgoing_verify_results_rx) = broadcast::channel(8);
                (
                    Some(ExternalNodeVerifierConfig {
                        signing_key,
                        verify_batch_tx,
                        outgoing_verify_results: outgoing_verify_results.clone(),
                    }),
                    Some(outgoing_verify_results),
                )
            } else {
                (None, None)
            };
        let state = HandlerSharedState::new(protocol_tx, max_active_connections);
        let connection_registry = Arc::new(RwLock::new(HashMap::new()));
        let (handler, verify_result_rx) = if node_role.is_main() {
            let (verify_result_tx, verify_result_rx) = mpsc::channel(8);
            (
                ZksProtocolHandler::<P, _>::for_main_node(
                    InMemReplay(HashMap::from_iter(replays)),
                    MainNodeProtocolConfig {
                        accepted_verifier_signers: accepted_verifier_signers(),
                        verify_result_tx,
                    },
                    state,
                    connection_registry.clone(),
                ),
                Some(verify_result_rx),
            )
        } else {
            (
                ZksProtocolHandler::<P, _>::for_external_node(
                    InMemReplay(HashMap::from_iter(replays)),
                    ExternalNodeProtocolConfig {
                        starting_block: Arc::new(RwLock::new(starting_block)),
                        record_overrides: vec![],
                        replay_sender: replay_tx,
                        verification,
                    },
                    state,
                    connection_registry.clone(),
                ),
                None,
            )
        };
        self.add_rlpx_sub_protocol(handler);
        TestPeerProtocolHandles {
            protocol_rx,
            replay_rx,
            verify_result_rx,
            outgoing_verify_results_tx,
        }
    }
}

#[test_casing(3, [ZksVersion::Zks1, ZksVersion::Zks2, ZksVersion::Zks3])]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_matching_version(version: ZksVersion) {
    // Run two peers that both communicate on exactly one matching zks protocol and successfully
    // transfer one replay record from peer0 to peer1.
    async fn test_inner<P: ZksProtocolVersionSpec>() {
        let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
        let record1 = dummy_record::<P>(1);

        let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<P>(
            NodeRole::MainNode,
            0,
            [(1, record1.clone())],
            100,
            false,
        );
        let (mut from_peer1, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<P>(
            NodeRole::ExternalNode,
            1,
            [(1, record1.clone())],
            100,
            false,
        );

        // Spawn and connect all the peers
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

    match version {
        ZksVersion::Zks0 => unreachable!(),
        ZksVersion::Zks1 => test_inner::<ZksProtocolV1>().await,
        ZksVersion::Zks2 => test_inner::<ZksProtocolV2>().await,
        ZksVersion::Zks3 => test_inner::<ZksProtocolV3>().await,
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn emits_replay_session_events() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        false,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        false,
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
            None => panic!("protocol event stream closed before replay events were observed"),
        }
    }

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn batches_multiple_replay_records_on_zks3() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);
    let record2 = dummy_record::<ZksProtocolV3>(2);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone()), (2, record2.clone())],
        100,
        false,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone()), (2, record2.clone())],
        100,
        false,
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
async fn emits_verifier_role_request_event() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        false,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        true,
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_verifier_role_requested = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_role_requested && saw_replay_requested) {
        match from_peer0.recv().await {
            Some(ProtocolEvent::VerifierRoleRequested { peer_id }) => {
                assert_eq!(peer_id, peer1_id);
                saw_verifier_role_requested = true;
            }
            Some(ProtocolEvent::ReplayRequested {
                peer_id,
                starting_block,
            }) => {
                assert_eq!(peer_id, peer1_id);
                assert_eq!(starting_block, 1);
                saw_replay_requested = true;
            }
            Some(ProtocolEvent::Established { .. } | ProtocolEvent::ReplayBlockSent { .. }) => {}
            Some(
                ProtocolEvent::VerifierChallengeSent { .. }
                | ProtocolEvent::VerifierAuthorized { .. }
                | ProtocolEvent::VerifierUnauthorized { .. },
            ) => {}
            Some(ProtocolEvent::Closed { .. }) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            None => panic!("protocol event stream closed before verifier role event was observed"),
        }
    }

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn authorizes_verifier_before_replay() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);
    let expected_signer = PrivateKeySigner::from_str(
        "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
    )
    .unwrap()
    .address();

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        false,
    );
    let (_, mut replay_rx_peer1) = net.peers_mut()[1].add_zks_sub_protocol::<ZksProtocolV3>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        true,
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_verifier_role_requested = false;
    let mut saw_verifier_challenge_sent = false;
    let mut saw_verifier_authorized = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_role_requested
        && saw_verifier_challenge_sent
        && saw_verifier_authorized
        && saw_replay_requested)
    {
        match from_peer0.recv().await {
            Some(ProtocolEvent::VerifierRoleRequested { peer_id }) => {
                assert_eq!(peer_id, peer1_id);
                saw_verifier_role_requested = true;
            }
            Some(ProtocolEvent::VerifierChallengeSent { peer_id, .. }) => {
                assert_eq!(peer_id, peer1_id);
                saw_verifier_challenge_sent = true;
            }
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
            Some(ProtocolEvent::Established { .. } | ProtocolEvent::ReplayBlockSent { .. }) => {}
            Some(ProtocolEvent::VerifierUnauthorized { signer, .. }) => {
                panic!("unexpected verifier unauthorized event: {signer:?}")
            }
            Some(ProtocolEvent::Closed { .. }) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            None => panic!("protocol event stream closed before verifier auth flow was observed"),
        }
    }

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn emits_verifier_unauthorized_before_replay() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);

    let mut main = net.peers_mut()[0].add_zks_sub_protocol_with_test_handles::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        None,
    );
    let mut external = net.peers_mut()[1].add_zks_sub_protocol_with_test_handles::<ZksProtocolV3>(
        NodeRole::ExternalNode,
        1,
        [(1, record1.clone())],
        100,
        Some(alternate_verifier_signing_key()),
    );

    let handle = net.spawn();
    handle.connect_peers().await;

    let peer1_id = *handle.peers()[1].peer_id();
    let mut saw_verifier_role_requested = false;
    let mut saw_verifier_challenge_sent = false;
    let mut saw_verifier_unauthorized = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_role_requested
        && saw_verifier_challenge_sent
        && saw_verifier_unauthorized
        && saw_replay_requested)
    {
        match main.protocol_rx.recv().await {
            Some(ProtocolEvent::VerifierRoleRequested { peer_id }) => {
                assert_eq!(peer_id, peer1_id);
                saw_verifier_role_requested = true;
            }
            Some(ProtocolEvent::VerifierChallengeSent { peer_id, .. }) => {
                assert_eq!(peer_id, peer1_id);
                saw_verifier_challenge_sent = true;
            }
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
                assert!(saw_verifier_unauthorized);
                saw_replay_requested = true;
            }
            Some(ProtocolEvent::Established { .. } | ProtocolEvent::ReplayBlockSent { .. }) => {}
            Some(ProtocolEvent::VerifierAuthorized { signer, .. }) => {
                panic!("unexpected verifier authorized event: {signer:?}")
            }
            Some(ProtocolEvent::Closed { .. }) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            None => {
                panic!("protocol event stream closed before verifier auth failure was observed")
            }
        }
    }

    let received_replay_record = external.replay_rx.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn forwards_verify_batch_result_to_main_node() {
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record::<ZksProtocolV3>(1);

    let mut main = net.peers_mut()[0].add_zks_sub_protocol_with_test_handles::<ZksProtocolV3>(
        NodeRole::MainNode,
        0,
        [(1, record1.clone())],
        100,
        None,
    );
    let mut external = net.peers_mut()[1].add_zks_sub_protocol_with_test_handles::<ZksProtocolV3>(
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
    let mut saw_verifier_authorized = false;
    let mut saw_replay_requested = false;

    while !(saw_verifier_authorized && saw_replay_requested) {
        match main.protocol_rx.recv().await {
            Some(ProtocolEvent::VerifierAuthorized { peer_id, .. }) => {
                assert_eq!(peer_id, external_peer_id);
                saw_verifier_authorized = true;
            }
            Some(ProtocolEvent::ReplayRequested {
                peer_id,
                starting_block,
            }) => {
                assert_eq!(peer_id, external_peer_id);
                assert_eq!(starting_block, 1);
                saw_replay_requested = true;
            }
            Some(
                ProtocolEvent::Established { .. }
                | ProtocolEvent::ReplayBlockSent { .. }
                | ProtocolEvent::VerifierRoleRequested { .. }
                | ProtocolEvent::VerifierChallengeSent { .. },
            ) => {}
            Some(ProtocolEvent::VerifierUnauthorized { signer, .. }) => {
                panic!("unexpected verifier unauthorized event: {signer:?}")
            }
            Some(ProtocolEvent::Closed { .. }) => {}
            Some(ProtocolEvent::MaxActiveConnectionsExceeded { .. }) => {
                panic!("unexpected max active connections event")
            }
            None => panic!("protocol event stream closed before verifier auth flow was observed"),
        }
    }

    let received_replay_record = external.replay_rx.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);

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

#[test_casing(3, [ZksVersion::Zks1, ZksVersion::Zks2, ZksVersion::Zks3])]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_different_versions(version: ZksVersion) {
    // Run two peers where peer0 can communicate on zks protocol v0 AND v1, while peer1 can only
    // communicate on v0. Test expects that they agree to communicate using v0 and manage to transfer
    // one replay record where all fields except block number are stripped (v0 only keeps block number
    // in tact).
    async fn test_inner<P: ZksProtocolVersionSpec>() {
        let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
        let record1 = dummy_record::<P>(1);
        let (_, _) = net.peers_mut()[0].add_zks_sub_protocol::<P>(
            NodeRole::MainNode,
            0,
            [(1, record1.clone())],
            100,
            false,
        );
        let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV0>(
            NodeRole::MainNode,
            0,
            [(1, record1.clone())],
            100,
            false,
        );

        let (mut from_peer1, mut replay_rx_peer1) = net.peers_mut()[1]
            .add_zks_sub_protocol::<ZksProtocolV0>(
                NodeRole::ExternalNode,
                1,
                [(1, record1.clone())],
                100,
                false,
            );

        // Spawn and connect all the peers
        let handle = net.spawn();
        handle.connect_peers().await;

        assert_matches!(from_peer0.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
            assert_eq!(peer_id, *handle.peers()[1].peer_id());
        });
        assert_matches!(from_peer1.recv().await, Some(ProtocolEvent::Established { peer_id, .. }) => {
            assert_eq!(peer_id, *handle.peers()[0].peer_id());
        });

        let received_replay_record = replay_rx_peer1.recv().await.unwrap();
        // Received record MUST NOT match what peer0 has in storage. This is expected because v0 loses
        // all record information except block number.
        assert_ne!(received_replay_record, record1);
        // This is the only field that is expected to match for v0.
        assert_eq!(
            received_replay_record.block_context.block_number,
            record1.block_context.block_number
        );
    }

    match version {
        ZksVersion::Zks0 => unreachable!(),
        ZksVersion::Zks1 => test_inner::<ZksProtocolV1>().await,
        ZksVersion::Zks2 => test_inner::<ZksProtocolV2>().await,
        ZksVersion::Zks3 => test_inner::<ZksProtocolV3>().await,
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn max_active_connections() {
    // Run three peers while peer0 has max active connections set to 1. peer1 is expected to be
    // successfully connected first while peer2 is expected to error out with
    // `MaxActiveConnectionsExceeded`.
    let mut net = Testnet::create_with(3, MockEthProvider::default()).await;

    let (mut from_peer0, _) = net.peers_mut()[0].add_zks_sub_protocol::<ZksProtocolV1>(
        NodeRole::MainNode,
        1,
        [],
        1,
        false,
    );

    let peer1 = &mut net.peers_mut()[1];
    let peer1_id = peer1.peer_id();
    let peer1_addr = peer1.local_addr();
    let (_, _) =
        peer1.add_zks_sub_protocol::<ZksProtocolV1>(NodeRole::ExternalNode, 1, [], 100, false);

    let peer2 = &mut net.peers_mut()[2];
    let peer2_id = peer2.peer_id();
    let peer2_addr = peer2.local_addr();
    let (_, _) =
        peer2.add_zks_sub_protocol::<ZksProtocolV1>(NodeRole::ExternalNode, 1, [], 100, false);

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
