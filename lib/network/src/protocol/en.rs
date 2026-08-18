use super::MAX_BLOCKS_PER_MESSAGE;
use super::ProtocolEvent;
use super::config::ExternalNodeProtocolConfig;
use crate::version::ZksProtocolVersionSpec;
use crate::wire::message::ZksMessage;
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zksync_os_storage_api::ReplayRecord;

/// Background task that drives the external-node side of a `zks` connection.
///
/// Sends a `GetBlockReplays` request immediately, then forwards each received `BlockReplays`
/// record to the local sequencer via `replay_sender` and advances `starting_block`.
///
/// Once the replay request is out, the connection has an inactivity timeout: if no message
/// arrives within `replay_inactivity_timeout` (or the message stream terminates while the RLPx
/// session stays up), a [`ProtocolEvent::ReplayStreamStalled`] is emitted so the service can
/// disconnect the peer and let a fresh session re-request replays. Without this, a session
/// whose data flow silently dies leaves the external node waiting forever.
pub(super) async fn run_en_connection<P: ZksProtocolVersionSpec>(
    conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    config: ExternalNodeProtocolConfig,
) {
    let ExternalNodeProtocolConfig {
        starting_block,
        record_overrides,
        max_blocks_per_message,
        replay_sender,
        verification: _,
        replay_inactivity_timeout,
    } = config;

    if send_replay_request::<P>(
        &outbound_tx,
        &starting_block,
        record_overrides,
        max_blocks_per_message,
    )
    .await
    .is_err()
    {
        return;
    }
    receive_replays(
        conn,
        starting_block,
        replay_sender,
        events_sender,
        peer_id,
        replay_inactivity_timeout,
    )
    .await;
}

async fn send_replay_request<P: ZksProtocolVersionSpec>(
    outbound_tx: &mpsc::Sender<BytesMut>,
    starting_block: &Arc<RwLock<BlockNumber>>,
    record_overrides: Vec<RecordOverride>,
    max_blocks_per_message: u64,
) -> Result<(), ()> {
    let next_block = *starting_block.read().unwrap();
    tracing::info!(next_block, "requesting block replays from main node");
    // The field remains optional to preserve the published `zks/5` request encoding. `None` is
    // valid on the wire and makes the main node fall back to one record per response.
    let max_blocks_per_message = Some(max_blocks_per_message.clamp(1, MAX_BLOCKS_PER_MESSAGE));
    let msg =
        ZksMessage::<P>::get_block_replays(next_block, max_blocks_per_message, record_overrides);
    outbound_tx.send(msg.encoded()).await.map_err(|_| ())
}

async fn receive_replays<P: ZksProtocolVersionSpec>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    starting_block: Arc<RwLock<BlockNumber>>,
    replay_sender: mpsc::Sender<ReplayRecord>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    inactivity_timeout: Duration,
) {
    let report_stalled = || {
        let next_block = *starting_block.read().unwrap();
        events_sender
            .send(ProtocolEvent::ReplayStreamStalled {
                peer_id,
                next_block,
            })
            .ok();
    };
    loop {
        // Measure time actually spent waiting on the peer.
        let inactivity_deadline = Instant::now() + inactivity_timeout;
        let msg = tokio::select! {
            msg = conn.next() => msg,
            _ = tokio::time::sleep_until(inactivity_deadline) => {
                // The session may still look established (RLPx pings can flow while the replay
                // stream is dead), so silence past the inactivity timeout is treated as a stall.
                tracing::warn!(
                    ?inactivity_timeout,
                    "no messages from replay peer within inactivity timeout; reporting stall"
                );
                report_stalled();
                return;
            }
        };
        let Some(msg) = msg else {
            // The message stream terminated (e.g. on a decode error) but the RLPx session may
            // still be alive; report a stall so the session gets torn down instead of lingering
            // half-dead with no replay flow.
            tracing::info!("replay message stream ended; reporting stall");
            report_stalled();
            return;
        };
        match msg {
            ZksMessage::GetBlockReplays(_) => {
                tracing::info!("ignoring request as local node is also waiting for records");
            }
            ZksMessage::BlockReplays(response) => {
                for record in response.records {
                    let block_number = record.block_number();
                    tracing::debug!(block_number, "received block replay");
                    let record: ReplayRecord = match record.try_into() {
                        Ok(record) => record,
                        Err(error) => {
                            tracing::info!(%error, "failed to recover replay block; reporting stall");
                            report_stalled();
                            return;
                        }
                    };

                    // Never panic on remote data: a panic here is swallowed by the runtime and
                    // leaves a zombie session with no inactivity monitoring. Stalling reconnects
                    // instead — transient causes self-heal, persistent ones fail loudly (and the
                    // sequencer pipeline still enforces the sequence as a hard backstop).
                    let expected_next_block = *starting_block.read().unwrap();
                    if block_number != expected_next_block {
                        tracing::warn!(
                            block_number,
                            expected_next_block,
                            "replay block out of sequence; reporting stall"
                        );
                        report_stalled();
                        return;
                    }

                    if replay_sender.send(record).await.is_err() {
                        tracing::trace!("network replay channel is closed");
                        return;
                    }
                    *starting_block.write().unwrap() += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ProtocolEvent;
    use crate::version::ZksProtocolV5;
    use alloy::primitives::B256;
    use assert_matches::assert_matches;
    use reth_network_peers::PeerId;
    use std::time::Duration;
    use zksync_os_metadata::NODE_SEMVER_VERSION;
    use zksync_os_storage_api::BlockContext;
    use zksync_os_types::{BlockStartCursors, ProtocolSemanticVersion};

    const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);

    struct TestEnConnection {
        outbound_rx: mpsc::Receiver<BytesMut>,
        replay_rx: mpsc::Receiver<ReplayRecord>,
        events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
        peer_id: PeerId,
    }

    fn run_test_en_connection(
        conn: impl Stream<Item = ZksMessage<ZksProtocolV5>> + Unpin + Send + 'static,
    ) -> (tokio::task::JoinHandle<()>, TestEnConnection) {
        let (outbound_tx, outbound_rx) = mpsc::channel(8);
        let (replay_tx, replay_rx) = mpsc::channel(8);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let peer_id = PeerId::random();
        let config = ExternalNodeProtocolConfig {
            starting_block: Arc::new(RwLock::new(1)),
            record_overrides: vec![],
            max_blocks_per_message: 64,
            replay_sender: replay_tx,
            verification: None,
            replay_inactivity_timeout: INACTIVITY_TIMEOUT,
        };
        let task = tokio::spawn(run_en_connection::<ZksProtocolV5>(
            conn,
            outbound_tx,
            events_tx,
            peer_id,
            config,
        ));
        (
            task,
            TestEnConnection {
                outbound_rx,
                replay_rx,
                events_rx,
                peer_id,
            },
        )
    }

    fn test_record(block_number: BlockNumber) -> ReplayRecord {
        ReplayRecord::new(
            BlockContext {
                block_number,
                ..Default::default()
            },
            vec![],
            24,
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
        )
    }

    #[tokio::test(start_paused = true)]
    async fn inactivity_timeout_terminates_idle_replay_connection() {
        // A session that stays established but never delivers anything, like the one observed
        // when the main node's outbound flow silently dies.
        let conn = futures::stream::pending::<ZksMessage<ZksProtocolV5>>();
        let (task, mut handles) = run_test_en_connection(conn);

        task.await.expect("connection task must finish on its own");

        // The replay request went out before the stream went quiet.
        assert!(handles.outbound_rx.try_recv().is_ok());
        assert_matches!(
            handles.events_rx.try_recv(),
            Ok(ProtocolEvent::ReplayStreamStalled { peer_id, next_block }) => {
                assert_eq!(peer_id, handles.peer_id);
                assert_eq!(next_block, 1);
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inactivity_timeout_resets_on_incoming_replays() {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
        let conn = Box::pin(futures::stream::poll_fn(move |cx| msg_rx.poll_recv(cx)));
        let (task, mut handles) = run_test_en_connection(conn);

        // Deliver replays with gaps shorter than the inactivity timeout; cumulative time exceeds
        // the timeout several times over, so this only passes if activity resets the deadline.
        for block_number in 1..=3 {
            tokio::time::sleep(INACTIVITY_TIMEOUT.mul_f64(0.7)).await;
            msg_tx
                .send(ZksMessage::block_replays(vec![test_record(block_number)]))
                .expect("connection task must still be reading");
            let forwarded = handles.replay_rx.recv().await.expect("record forwarded");
            assert_eq!(forwarded.block_context.block_number, block_number);
        }
        assert!(
            !task.is_finished(),
            "inactivity timeout must not fire while replays keep arriving"
        );

        // Now go silent: the inactivity timeout terminates the connection from the next block.
        task.await.expect("connection task must finish on its own");
        assert_matches!(
            handles.events_rx.try_recv(),
            Ok(ProtocolEvent::ReplayStreamStalled { peer_id, next_block }) => {
                assert_eq!(peer_id, handles.peer_id);
                assert_eq!(next_block, 4);
            }
        );
    }

    #[tokio::test]
    async fn stream_end_emits_stalled_event() {
        // A decode error terminates the message stream while the RLPx session stays up; the EN
        // must report the stall so the session gets torn down instead of lingering half-dead.
        let conn = futures::stream::empty::<ZksMessage<ZksProtocolV5>>();
        let (task, mut handles) = run_test_en_connection(conn);

        task.await.expect("connection task must finish on its own");
        assert_matches!(
            handles.events_rx.try_recv(),
            Ok(ProtocolEvent::ReplayStreamStalled { peer_id, next_block }) => {
                assert_eq!(peer_id, handles.peer_id);
                assert_eq!(next_block, 1);
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn out_of_sequence_record_emits_stalled_event() {
        // The cursor starts at 1, so a record for block 5 breaks the sequence. The stream then
        // stays pending: the task must finish via the sequence check, not via stream end.
        let msg = ZksMessage::block_replays(vec![test_record(5)]);
        let conn = Box::pin(futures::stream::iter([msg]).chain(futures::stream::pending()));
        let (task, mut handles) = run_test_en_connection(conn);

        task.await.expect("connection task must finish on its own");
        assert_matches!(
            handles.events_rx.try_recv(),
            Ok(ProtocolEvent::ReplayStreamStalled { peer_id, next_block }) => {
                assert_eq!(peer_id, handles.peer_id);
                assert_eq!(next_block, 1);
            }
        );
        assert!(
            handles.replay_rx.try_recv().is_err(),
            "out-of-sequence record must not be forwarded to the sequencer"
        );
    }
}
