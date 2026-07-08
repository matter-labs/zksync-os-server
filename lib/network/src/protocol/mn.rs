use super::MAX_BLOCKS_PER_MESSAGE;
use super::ProtocolEvent;
use super::config::MainNodeProtocolConfig;
use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::version::ZksProtocolVersionSpec;
use crate::wire::auth::{VerifierAuth, recover_verifier_signer, verifier_auth_prehash};
use crate::wire::message::{ZksMessage, ZksMessageId};
use alloy::primitives::B256;
use alloy::primitives::bytes::BytesMut;
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use futures::stream::BoxStream;
use futures::{FutureExt, Stream, StreamExt};
use reth_network_peers::PeerId;
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};
use zksync_os_storage_api::{ReadReplay, ReadReplayExt, ReplayRecord};

/// Background task that drives a main-node side of a connection.
///
/// The peer decides what the session carries, message by message:
///
/// - An external node sends `GetBlockReplays`; from then on this side streams
///   replay records indefinitely.
/// - A verifier peer requests the verifier role (challenge-response against the
///   accepted-signer list) and later returns `VerifyBatchResult`s for dispatched
///   requests.
/// - When this node itself carries a verifier attachment (a committee validator
///   acting as a batch verifier), it *also* requests the verifier role from the
///   peer and answers incoming `VerifyBatch` requests — so two validators verify
///   for each other over one session regardless of who dialed whom.
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
        verification,
    } = config;

    // Our own verifier half, when attached: request the role up front; the
    // signer answers the peer's challenge below.
    let our_signer = verification.as_ref().and_then(|verifier| {
        match PrivateKeySigner::from_str(verifier.signing_key.expose_secret()) {
            Ok(signer) => Some(signer),
            Err(error) => {
                tracing::info!(%error, "invalid verifier signing key; not requesting verifier role");
                None
            }
        }
    });
    if our_signer.is_some() && P::VERSION.supports_message(ZksMessageId::VerifierRoleRequest) {
        let msg = ZksMessage::<P>::VerifierRoleRequest(Default::default());
        if outbound_tx.send(msg.encoded()).await.is_err() {
            return;
        }
    }
    let mut outgoing_verify_results = verification
        .as_ref()
        .map(|verifier| verifier.outgoing_verify_results.subscribe());

    // Challenge we issued to the peer (its role request), awaiting its auth.
    let mut pending_verifier_nonce: Option<B256> = None;
    // Armed once the peer requests replays; `None` for verifier-only sessions.
    let mut replay_stream: Option<(BoxStream<'static, ReplayRecord>, usize)> = None;

    loop {
        tokio::select! {
            // Biased: control messages first — the replay stream is unbounded
            // and would otherwise starve them.
            biased;

            msg = conn.next() => {
                let Some(msg) = msg else {
                    tracing::info!("peer connection closed; terminating");
                    return;
                };
                match msg {
                    ZksMessage::VerifierRoleRequest(_) => {
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
                    ZksMessage::VerifierAuth(auth) => {
                        let Some(nonce) = pending_verifier_nonce.take() else {
                            tracing::info!(
                                "received verifier auth without pending challenge; terminating"
                            );
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
                    // The peer challenges *our* role request.
                    ZksMessage::VerifierChallenge(challenge) => {
                        let Some(signer) = &our_signer else {
                            tracing::info!(
                                "received verifier challenge without a verifier attachment; ignoring"
                            );
                            continue;
                        };
                        let signature =
                            match signer.sign_hash_sync(&verifier_auth_prehash(challenge.nonce)) {
                                Ok(signature) => signature,
                                Err(error) => {
                                    tracing::info!(%error, "failed to sign verifier challenge");
                                    continue;
                                }
                            };
                        let msg = ZksMessage::<P>::VerifierAuth(VerifierAuth {
                            signature: signature.as_bytes().to_vec().into(),
                        });
                        if outbound_tx.send(msg.encoded()).await.is_err() {
                            return;
                        }
                    }
                    // The peer (a settler) asks us to verify one of its batches.
                    ZksMessage::VerifyBatch(request) => {
                        let Some(verifier) = &verification else {
                            tracing::info!(
                                "ignoring verify batch request; verifier transport not configured"
                            );
                            continue;
                        };
                        if verifier
                            .verify_batch_tx
                            .send(PeerVerifyBatch {
                                peer_id,
                                message: request,
                            })
                            .await
                            .is_err()
                        {
                            tracing::info!("verify batch channel is closed; terminating");
                            return;
                        }
                    }
                    ZksMessage::VerifyBatchResult(result) => {
                        tracing::debug!(
                            %peer_id,
                            request_id = result.request_id,
                            "received verify result from peer"
                        );
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
                    ZksMessage::GetBlockReplays(request) if replay_stream.is_none() => {
                        events_sender
                            .send(ProtocolEvent::ReplayRequested {
                                peer_id,
                                starting_block: request.starting_block,
                            })
                            .ok();
                        let max_blocks_per_message = request
                            .max_blocks_per_message
                            .unwrap_or(1)
                            .clamp(1, MAX_BLOCKS_PER_MESSAGE)
                            as usize;
                        let stream = replay
                            .clone()
                            .stream_from_forever(request.starting_block, HashMap::new())
                            .boxed();
                        replay_stream = Some((stream, max_blocks_per_message));
                    }
                    msg => {
                        tracing::info!(?msg, "received unexpected message from peer; terminating");
                        return;
                    }
                }
            }

            // Stream records to the EN, once requested.
            record = async { replay_stream.as_mut().expect("guarded").0.next().await },
                if replay_stream.is_some() =>
            {
                let Some(record) = record else {
                    // stream_from_forever only ends if storage closes.
                    tracing::info!("replay stream closed; terminating");
                    return;
                };
                let (stream, max_blocks_per_message) =
                    replay_stream.as_mut().expect("guarded above");
                let mut records = vec![record];
                let mut replay_stream_closed = false;
                while records.len() < *max_blocks_per_message {
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

            // Our verifier half's signatures, routed back to the requesting peer.
            result = recv_outgoing_verify_result(&mut outgoing_verify_results) => {
                let Some(result) = result else {
                    continue;
                };
                if result.peer_id != peer_id {
                    continue;
                }
                tracing::debug!(
                    %peer_id,
                    request_id = result.message.request_id,
                    "forwarding verify result to the requesting peer"
                );
                if outbound_tx
                    .send(ZksMessage::<P>::VerifyBatchResult(result.message).encoded())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn recv_outgoing_verify_result(
    receiver: &mut Option<broadcast::Receiver<PeerVerifyBatchResult>>,
) -> Option<PeerVerifyBatchResult> {
    let receiver = match receiver {
        Some(receiver) => receiver,
        None => {
            std::future::pending::<()>().await;
            unreachable!();
        }
    };
    loop {
        match receiver.recv().await {
            Ok(result) => return Some(result),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "lagged on outgoing verify results broadcast");
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}
