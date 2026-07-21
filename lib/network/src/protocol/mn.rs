use super::MAX_BLOCKS_PER_MESSAGE;
use super::ProtocolEvent;
use crate::version::ZksProtocolVersionSpec;
use crate::wire::message::ZksMessage;
use alloy::primitives::bytes::BytesMut;
use futures::{FutureExt, Stream, StreamExt};
use reth_network_peers::PeerId;
use std::collections::HashMap;
use tokio::sync::mpsc;
use zksync_os_storage_api::{ReadReplay, ReadReplayExt};

/// Background task that drives the main-node side of a `zks` connection.
///
/// Waits for a `GetBlockReplays` request from the EN, then streams replay records from storage to
/// the EN indefinitely.
pub(super) async fn run_mn_connection<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    replay: Replay,
) {
    // Receive the single GetBlockReplays request for this connection.
    let request = match conn.next().await {
        Some(ZksMessage::GetBlockReplays(request)) => request,
        Some(msg) => {
            tracing::info!(
                ?msg,
                "received unexpected initial message from peer; terminating"
            );
            return;
        }
        None => return,
    };
    events_sender
        .send(ProtocolEvent::ReplayRequested {
            peer_id,
            starting_block: request.starting_block,
        })
        .ok();
    let max_blocks_per_message = request
        .max_blocks_per_message
        .unwrap_or(1)
        .clamp(1, MAX_BLOCKS_PER_MESSAGE) as usize;
    // Overrides let a debugging EN sync reverted records that are stored under non-canonical
    // db keys (see `en_replay_record_overrides` config).
    let db_key_overrides: HashMap<_, _> = request
        .record_overrides
        .into_iter()
        .map(|record_override| {
            (
                record_override.block_number,
                record_override.db_key.to_vec(),
            )
        })
        .collect();

    // Stream records to the EN indefinitely.
    let mut stream = replay
        .clone()
        .stream_from_forever(request.starting_block, db_key_overrides);
    loop {
        tokio::select! {
            // Biased because first branch always leads to early return. Makes sense to check it
            // first.
            biased;

            msg = conn.next() => {
                match msg {
                    Some(msg) => {
                        tracing::info!(?msg, "received unexpected message from peer; terminating");
                        return;
                    }
                    None => {
                        tracing::info!("peer connection closed; terminating");
                        return;
                    }
                }
            }
            record = stream.next() => {
                let Some(record) = record else {
                    // stream_from_forever only ends if storage closes.
                    tracing::info!("replay stream closed; terminating");
                    return;
                };
                let mut records = vec![record];
                let mut replay_stream_closed = false;
                while records.len() < max_blocks_per_message {
                    match stream.next().now_or_never() {
                        Some(Some(record)) => records.push(record),
                        Some(None) => {
                            replay_stream_closed = true;
                            break;
                        }
                        None => break,
                    }
                }
                let block_numbers: Vec<_> = records
                    .iter()
                    .map(|record| record.block_context.block_number)
                    .collect();
                let encoded = ZksMessage::<P>::block_replays(records).encoded();
                if outbound_tx.send(encoded).await.is_err() {
                    return;
                }
                for block_number in block_numbers {
                    events_sender
                        .send(ProtocolEvent::ReplayBlockSent {
                            peer_id,
                            block_number,
                        })
                        .ok();
                }
                if replay_stream_closed {
                    tracing::info!("replay stream closed; terminating");
                    return;
                }
            }
        }
    }
}
