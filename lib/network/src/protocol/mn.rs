use super::MAX_BLOCKS_PER_MESSAGE;
use super::ProtocolEvent;
use super::config::MainNodeProtocolConfig;
use crate::service::PeerVerifyBatchResult;
use crate::version::ZksProtocolVersionSpec;
use crate::wire::auth::recover_verifier_signer;
use crate::wire::message::ZksMessage;
use alloy::primitives::B256;
use alloy::primitives::bytes::BytesMut;
use futures::{FutureExt, Stream, StreamExt};
use reth_network_peers::PeerId;
use std::collections::HashMap;
use tokio::sync::mpsc;
use zksync_os_storage_api::{ReadReplay, ReadReplayExt};

/// Background task that drives a main-node side of a connection.
///
/// Waits for a `GetBlockReplays` request from the EN, then streams replay records from storage to
/// the EN indefinitely.
pub(super) async fn run_mn_connection<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    replay: Replay,
    config: MainNodeProtocolConfig,
) {
    let MainNodeProtocolConfig {
        accepted_verifier_signers,
        verify_result_tx,
    } = config;
    let mut pending_verifier_nonce: Option<B256> = None;
    // Receive the single GetBlockReplays request for this connection. On zks/3, verifier ENs may
    // opt into verifier role before replay starts.
    let request = loop {
        match conn.next().await {
            Some(ZksMessage::VerifierRoleRequest(_)) => {
                events_sender
                    .send(ProtocolEvent::VerifierRoleRequested { peer_id })
                    .ok();
                let nonce = B256::random();
                if outbound_tx
                    .send(ZksMessage::<P>::verifier_challenge(nonce).encoded())
                    .await
                    .is_err()
                {
                    return;
                }
                pending_verifier_nonce = Some(nonce);
                events_sender
                    .send(ProtocolEvent::VerifierChallengeSent { peer_id, nonce })
                    .ok();
            }
            Some(ZksMessage::VerifierAuth(auth)) => {
                let Some(nonce) = pending_verifier_nonce.take() else {
                    tracing::info!("received verifier auth without pending challenge; terminating");
                    return;
                };
                match recover_verifier_signer(nonce, auth.signature.as_ref()) {
                    Ok(signer) if accepted_verifier_signers.contains(&signer) => {
                        events_sender
                            .send(ProtocolEvent::VerifierAuthorized { peer_id, signer })
                            .ok();
                    }
                    Ok(signer) => {
                        tracing::warn!(%peer_id, %signer, "peer failed verifier authorization");
                        events_sender
                            .send(ProtocolEvent::VerifierUnauthorized {
                                peer_id,
                                signer: Some(signer),
                            })
                            .ok();
                    }
                    Err(error) => {
                        tracing::warn!(%peer_id, %error, "failed to recover verifier signer");
                        events_sender
                            .send(ProtocolEvent::VerifierUnauthorized {
                                peer_id,
                                signer: None,
                            })
                            .ok();
                    }
                }
            }
            Some(ZksMessage::GetBlockReplays(request)) => break request,
            Some(msg) => {
                tracing::info!(
                    ?msg,
                    "received unexpected initial message from peer; terminating"
                );
                return;
            }
            None => return,
        }
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

    // Stream records to the EN indefinitely.
    let mut stream = replay
        .clone()
        .stream_from_forever(request.starting_block, HashMap::new());
    loop {
        tokio::select! {
            // Biased because first branch always leads to early return. Makes sense to check it
            // first.
            biased;

            msg = conn.next() => {
                match msg {
                    Some(ZksMessage::VerifyBatchResult(result)) => {
                        if verify_result_tx
                            .send(PeerVerifyBatchResult {
                                peer_id,
                                message: result,
                            })
                            .await
                            .is_err()
                        {
                            tracing::info!("verify result channel is closed; terminating");
                            return;
                        }
                    }
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
