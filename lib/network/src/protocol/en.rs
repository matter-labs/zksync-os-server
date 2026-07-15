use super::MAX_BLOCKS_PER_MESSAGE;
use super::ProtocolEvent;
use super::config::{ExternalNodeProtocolConfig, ExternalNodeVerifierConfig};
use super::upstream::UpstreamGuard;
use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::version::ZksProtocolVersionSpec;
use crate::wire::auth::{VerifierAuth, verifier_auth_prehash};
use crate::wire::message::{ZksMessage, ZksMessageId};
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use alloy::primitives::bytes::BytesMut;
use alloy::primitives::{B256, BlockNumber};
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use futures::FutureExt;
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use tokio::sync::{OwnedSemaphorePermit, broadcast, mpsc};
use zksync_os_storage_api::{ReadReplay, ReadReplayExt, ReplayRecord};

/// Background task that drives an external-node side of a connection.
///
/// Per-connection sync-state machine: probes the peer for blocks, pins the first cursor-aligned
/// one as the single upstream (via [`UpstreamGuard`]), and serves all other peers from the local WAL.
pub(super) async fn run_en_connection<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    replay: Replay,
    config: ExternalNodeProtocolConfig,
    upstream: UpstreamGuard,
) {
    let ExternalNodeProtocolConfig {
        starting_block,
        record_overrides,
        max_blocks_per_message,
        replay_sender,
        verification: verifier,
    } = config;

    if perform_verifier_handshake::<P>(&mut conn, &outbound_tx, verifier.as_ref())
        .await
        .is_err()
    {
        return;
    }

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

    receive_and_serve_replays(
        conn,
        outbound_tx,
        starting_block,
        replay_sender,
        peer_id,
        verifier,
        upstream,
        events_sender,
        replay,
    )
    .await;
}

async fn perform_verifier_handshake<P: ZksProtocolVersionSpec>(
    conn: &mut (impl Stream<Item = ZksMessage<P>> + Unpin),
    outbound_tx: &mpsc::Sender<BytesMut>,
    verifier: Option<&ExternalNodeVerifierConfig>,
) -> Result<(), ()> {
    let Some(verifier) = verifier else {
        return Ok(());
    };
    if !P::VERSION.supports_message(ZksMessageId::VerifierRoleRequest) {
        return Ok(());
    }

    let msg = ZksMessage::<P>::VerifierRoleRequest(Default::default());
    if outbound_tx.send(msg.encoded()).await.is_err() {
        return Err(());
    }

    let signer = match PrivateKeySigner::from_str(verifier.signing_key.expose_secret()) {
        Ok(signer) => signer,
        Err(error) => {
            tracing::info!(%error, "invalid verifier signing key; terminating");
            return Err(());
        }
    };

    let challenge = match conn.next().await {
        Some(ZksMessage::VerifierChallenge(challenge)) => challenge,
        Some(other) => {
            // With EN-EN, two ENs can both send `VerifierRoleRequest` simultaneously; one EN's
            // `GetBlockReplays` probe can race ahead and arrive before its `VerifierChallenge`.
            tracing::info!(
                ?other,
                "received unexpected message while waiting for verifier challenge; skipping handshake"
            );
            return Ok(());
        }
        None => return Err(()),
    };

    let signature = match signer.sign_hash_sync(&verifier_auth_prehash(challenge.nonce)) {
        Ok(signature) => signature,
        Err(error) => {
            tracing::info!(%error, "failed to sign verifier challenge; terminating");
            return Err(());
        }
    };

    let msg = ZksMessage::<P>::VerifierAuth(VerifierAuth {
        signature: signature.as_bytes().to_vec().into(),
    });
    if outbound_tx.send(msg.encoded()).await.is_err() {
        return Err(());
    }
    Ok(())
}

async fn send_replay_request<P: ZksProtocolVersionSpec>(
    outbound_tx: &mpsc::Sender<BytesMut>,
    starting_block: &Arc<RwLock<BlockNumber>>,
    record_overrides: Vec<RecordOverride>,
    max_blocks_per_message: u64,
) -> Result<(), ()> {
    let next_block = *starting_block.read().unwrap();
    tracing::info!(next_block, "requesting block replays");
    let max_blocks_per_message = P::VERSION
        .supports_message(ZksMessageId::VerifierRoleRequest)
        .then_some(max_blocks_per_message.clamp(1, MAX_BLOCKS_PER_MESSAGE));
    let msg =
        ZksMessage::<P>::get_block_replays(next_block, max_blocks_per_message, record_overrides);
    outbound_tx.send(msg.encoded()).await.map_err(|_| ())
}

async fn receive_and_serve_replays<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    starting_block: Arc<RwLock<BlockNumber>>,
    replay_sender: mpsc::Sender<ReplayRecord>,
    peer_id: PeerId,
    verifier: Option<ExternalNodeVerifierConfig>,
    upstream: UpstreamGuard,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    replay: Replay,
) {
    let mut upstream_permit: Option<OwnedSemaphorePermit> = None;
    let mut serve_stream: Option<(Pin<Box<dyn Stream<Item = ReplayRecord> + Send>>, usize)> = None;
    let mut pending_verifier_nonce: Option<B256> = None;
    let mut outgoing_verify_results = verifier
        .as_ref()
        .map(|verifier| verifier.outgoing_verify_results.subscribe());

    'main: loop {
        tokio::select! {
            // Inbound wire messages from the peer.
            msg = conn.next() => {
                let Some(msg) = msg else {
                    break 'main;
                };
                match msg {
                    ZksMessage::BlockReplays(response) => {
                        if upstream_permit.is_none() {
                            // Skip, if response is empty.
                            let Some(first_block) =
                                response.records.first().map(|record| record.block_number())
                            else {
                                continue;
                            };
                            // Skip, if response is not aligned with the local cursor.
                            // This rejects a stale stream left over from before we synced through another link.
                            if first_block != *starting_block.read().unwrap() {
                                continue;
                            }
                            // Try to pin this peer as the single upstream,
                            // otherwise - skip.
                            match upstream.try_acquire(peer_id) {
                                Some(acquired) => {
                                    upstream_permit = Some(acquired);
                                    serve_stream = None;
                                }
                                None => continue,
                            }
                        }
                        // Consume the stream of replays.
                        for record in response.records {
                            let block_number = record.block_number();
                            tracing::debug!(block_number, "received block replay");
                            let record: ReplayRecord = match record.try_into() {
                                Ok(record) => record,
                                Err(error) => {
                                    tracing::info!(%error, "failed to recover replay block");
                                    break 'main;
                                }
                            };
                            let expected_next_block = *starting_block.read().unwrap();
                            if block_number != expected_next_block {
                                tracing::warn!(
                                    block_number,
                                    expected_next_block,
                                    "upstream sent out-of-order block; disconnecting"
                                );
                                break 'main;
                            }
                            if replay_sender.send(record).await.is_err() {
                                tracing::trace!("network replay channel is closed");
                                break 'main;
                            }
                            *starting_block.write().unwrap() += 1;
                        }
                    }
                    ZksMessage::GetBlockReplays(request) => {
                        // Serve any peer that is not our pinned upstream (loop prevention). An empty
                        // store streams nothing until it holds the requested blocks.
                        if upstream.peer_id() != Some(peer_id) {
                            events_sender
                                .send(ProtocolEvent::ReplayRequested {
                                    peer_id,
                                    starting_block: request.starting_block,
                                })
                                .ok();
                            let max_batch = request
                                .max_blocks_per_message
                                .unwrap_or(1)
                                .clamp(1, MAX_BLOCKS_PER_MESSAGE) as usize;
                            serve_stream = Some((
                                Box::pin(
                                    replay.clone().stream_from_forever(
                                        request.starting_block,
                                        HashMap::new(),
                                    ),
                                ),
                                max_batch,
                            ));
                        }
                    }
                    ZksMessage::VerifyBatch(request) => match &verifier {
                        Some(v) => {
                            if v.verify_batch_tx
                                .send(PeerVerifyBatch {
                                    peer_id,
                                    message: request,
                                })
                                .await
                                .is_err()
                            {
                                tracing::info!("verify batch channel is closed; terminating");
                                break 'main;
                            }
                        }
                        None => tracing::info!(
                            "ignoring verify batch request; verifier transport not configured"
                        ),
                    },
                    ZksMessage::VerifierRoleRequest(_) => {
                        // A downstream peer is attempting the verifier handshake. Complete it so
                        // the peer's perform_verifier_handshake does not terminate the connection,
                        // but accept no signers: the peer will fail auth and replay still streams.
                        let nonce = B256::random();
                        if outbound_tx
                            .send(ZksMessage::<P>::verifier_challenge(nonce).encoded())
                            .await
                            .is_err()
                        {
                            break 'main;
                        }
                        pending_verifier_nonce = Some(nonce);
                    }
                    ZksMessage::VerifierAuth(_) => {
                        // Auth is always rejected on the EN serve path; just clear the nonce.
                        pending_verifier_nonce.take();
                    }
                    ZksMessage::VerifierChallenge(_) => {
                        tracing::info!("ignoring unexpected verifier challenge");
                    }
                    ZksMessage::VerifyBatchResult(_) => {
                        tracing::info!(
                            "ignoring verify batch result; EN has no verifier result transport"
                        );
                    }
                }
            }
            // Outbound WAL records to the peer.
            record = next_serve_record(&mut serve_stream) => {
                match record {
                    Some((first, max_batch)) => {
                        let mut records = vec![first];
                        if let Some((stream, _)) = serve_stream.as_mut() {
                            while records.len() < max_batch {
                                match stream.next().now_or_never() {
                                    Some(Some(r)) => records.push(r),
                                    _ => break,
                                }
                            }
                        }
                        let block_numbers: Vec<_> = records
                            .iter()
                            .map(|r| r.block_context.block_number)
                            .collect();
                        if outbound_tx
                            .send(ZksMessage::<P>::block_replays(records).encoded())
                            .await
                            .is_err()
                        {
                            break 'main;
                        }
                        for block_number in block_numbers {
                            events_sender
                                .send(ProtocolEvent::ReplayBlockSent {
                                    peer_id,
                                    block_number,
                                })
                                .ok();
                        }
                    }
                    None => serve_stream = None,
                }
            }
            // Outbound verify results from the internal verifier to the peer.
            result = recv_outgoing_verify_result(&mut outgoing_verify_results) => {
                if let Some(result) = result
                    && result.peer_id == peer_id
                    && outbound_tx
                        .send(ZksMessage::<P>::VerifyBatchResult(result.message).encoded())
                        .await
                        .is_err()
                {
                    break 'main;
                }
            }
        }
    }

    // Release the upstream slot so another link can be pinned.
    if upstream_permit.is_some() {
        upstream.clear();
    }
}

/// Awaits the first record to serve, returning it alongside the batch size. Returns `None` if the
/// stream ends. Never resolves when not serving, so the select arm stays idle.
async fn next_serve_record(
    serve_stream: &mut Option<(Pin<Box<dyn Stream<Item = ReplayRecord> + Send>>, usize)>,
) -> Option<(ReplayRecord, usize)> {
    match serve_stream.as_mut() {
        Some((stream, max_batch)) => stream.next().await.map(|r| (r, *max_batch)),
        None => std::future::pending().await,
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
