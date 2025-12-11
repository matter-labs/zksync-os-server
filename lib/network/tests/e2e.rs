use alloy::primitives::{B256, BlockNumber};
use reth_network::{Peers, test_utils::Testnet};
use reth_provider::test_utils::MockEthProvider;
use std::collections::HashMap;
use tokio::sync::mpsc;
use zksync_os_interface::types::BlockContext;
use zksync_os_network::protocol::{ProtocolEvent, ProtocolState, ZksProtocolHandler};
use zksync_os_network::version::{ZksProtocolV0, ZksProtocolV1};
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::ProtocolSemanticVersion;

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

fn dummy_record(block_number: BlockNumber) -> ReplayRecord {
    ReplayRecord::new(
        BlockContext {
            block_number,
            ..Default::default()
        },
        42,
        vec![],
        24,
        // Important that this is set to 0.0.0 as v1 does not transport node version over the network
        // and 0.0.0 is the fallback value.
        semver::Version::new(0, 0, 0),
        ProtocolSemanticVersion::new(4, 5, 6),
        B256::random(),
        vec![],
    )
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_matching_version() {
    // Run two peers that both communicate on zks protocol v1 and successfully transfer one replay
    // record from peer0 to peer1.
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record(1);

    let (protocol_tx, mut from_peer0) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer0) = mpsc::unbounded_channel();
    let peer0 = &mut net.peers_mut()[0];
    peer0.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 100,
        replay: InMemReplay(HashMap::from([(1, record1.clone())])),
        to_request_blocks: false,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let (protocol_tx, mut from_peer1) = mpsc::unbounded_channel();
    let (replay_tx, mut replay_rx_peer1) = mpsc::unbounded_channel();
    let peer1 = &mut net.peers_mut()[1];
    peer1.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 100,
        replay: InMemReplay::default(),
        to_request_blocks: true,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    // Spawn and connect all the peers
    let handle = net.spawn();
    handle.connect_peers().await;

    match from_peer0.recv().await.unwrap() {
        ProtocolEvent::Established { peer_id, .. } => {
            assert_eq!(peer_id, *handle.peers()[1].peer_id());
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    };
    match from_peer1.recv().await.unwrap() {
        ProtocolEvent::Established { peer_id, .. } => {
            assert_eq!(peer_id, *handle.peers()[0].peer_id());
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    };

    let received_replay_record = replay_rx_peer1.recv().await.unwrap();
    assert_eq!(received_replay_record, record1);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn send_replay_record_different_versions() {
    // Run two peers where peer0 can communicate on zks protocol v0 AND v1, while peer1 can only
    // communicate on v0. Test expects that they agree to communicate using v0 and manage to transfer
    // one replay record where all fields except block number are stripped (v0 only keeps block number
    // in tact).
    let mut net = Testnet::create_with(2, MockEthProvider::default()).await;
    let record1 = dummy_record(1);

    let (protocol_tx, _from_peer0) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer0) = mpsc::unbounded_channel();
    let peer0 = &mut net.peers_mut()[0];
    peer0.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 100,
        replay: InMemReplay(HashMap::from([(1, record1.clone())])),
        to_request_blocks: false,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let (protocol_tx, mut from_peer0) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer0) = mpsc::unbounded_channel();
    peer0.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV0, _> {
        max_active_connections: 100,
        replay: InMemReplay(HashMap::from([(1, record1.clone())])),
        to_request_blocks: false,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let (protocol_tx, mut from_peer1) = mpsc::unbounded_channel();
    let (replay_tx, mut replay_rx_peer1) = mpsc::unbounded_channel();
    let peer1 = &mut net.peers_mut()[1];
    peer1.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV0, _> {
        max_active_connections: 100,
        replay: InMemReplay::default(),
        to_request_blocks: true,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    // Spawn and connect all the peers
    let handle = net.spawn();
    handle.connect_peers().await;

    match from_peer0.recv().await.unwrap() {
        ProtocolEvent::Established { peer_id, .. } => {
            assert_eq!(peer_id, *handle.peers()[1].peer_id());
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    };
    match from_peer1.recv().await.unwrap() {
        ProtocolEvent::Established { peer_id, .. } => {
            assert_eq!(peer_id, *handle.peers()[0].peer_id());
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    };

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

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn max_active_connections() {
    // Run three peers while peer0 has max active connections set to 1. peer1 is expected to be
    // successfully connected first while peer2 is expected to error out with
    // `MaxActiveConnectionsExceeded`.
    let mut net = Testnet::create_with(3, MockEthProvider::default()).await;

    let (protocol_tx, mut from_peer0) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer0) = mpsc::unbounded_channel();
    let peer0 = &mut net.peers_mut()[0];
    peer0.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 1,
        replay: InMemReplay::default(),
        to_request_blocks: false,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let (protocol_tx, _from_peer1) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer1) = mpsc::unbounded_channel();
    let peer1 = &mut net.peers_mut()[1];
    let peer1_id = peer1.peer_id();
    let peer1_addr = peer1.local_addr();
    peer1.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 100,
        replay: InMemReplay::default(),
        to_request_blocks: true,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let (protocol_tx, _from_peer2) = mpsc::unbounded_channel();
    let (replay_tx, _replay_rx_peer2) = mpsc::unbounded_channel();
    let peer2 = &mut net.peers_mut()[2];
    let peer2_id = peer2.peer_id();
    let peer2_addr = peer2.local_addr();
    peer2.add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
        max_active_connections: 100,
        replay: InMemReplay::default(),
        to_request_blocks: true,
        state: ProtocolState::new(protocol_tx),
        replay_sender: replay_tx,
        _phantom: Default::default(),
    });

    let handle = net.spawn();

    // Connect peers 0 and 1
    let peer0_handle = &handle.peers()[0];
    peer0_handle.network().add_peer(peer1_id, peer1_addr);
    match from_peer0.recv().await.unwrap() {
        ProtocolEvent::Established { peer_id, .. } => {
            assert_eq!(peer_id, *peer1_id);
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    }

    // Connect peers 0 and 2, max active connections exceeded
    peer0_handle.network().add_peer(peer2_id, peer2_addr);
    match from_peer0.recv().await.unwrap() {
        ProtocolEvent::MaxActiveConnectionsExceeded { num_active } => {
            assert_eq!(num_active, 1);
        }
        ev => {
            panic!("unexpected event: {ev:?}");
        }
    }
}
