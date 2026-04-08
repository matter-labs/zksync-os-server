use super::MAX_BLOCKS_PER_MESSAGE;
use super::config::{ExternalNodeProtocolConfig, ExternalNodeVerifierConfig};
use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::version::ZksProtocolVersionSpec;
use crate::wire::auth::{VerifierAuth, verifier_auth_prehash};
use crate::wire::message::{ZksMessage, ZksMessageId};
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use secrecy::ExposeSecret;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};
use zksync_os_storage_api::ReplayRecord;

/// Background task that drives an external-node side of a connection.
///
/// Sends a `GetBlockReplays` request immediately, then forwards each received `BlockReplays`
/// record to the local sequencer via `replay_sender` and advances `starting_block`.
pub(super) async fn run_en_connection<P: ZksProtocolVersionSpec>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    peer_id: PeerId,
    config: ExternalNodeProtocolConfig,
) {
    let ExternalNodeProtocolConfig {
        starting_block,
        record_overrides,
        replay_sender,
        verification: verifier,
    } = config;
    if perform_verifier_handshake::<P>(&mut conn, &outbound_tx, verifier.as_ref())
        .await
        .is_err()
    {
        return;
    }

    if send_replay_request::<P>(&outbound_tx, &starting_block, record_overrides)
        .await
        .is_err()
    {
        return;
    }
    receive_replay_and_verification(
        conn,
        outbound_tx,
        starting_block,
        replay_sender,
        peer_id,
        verifier,
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
            tracing::info!(
                ?other,
                "received unexpected message while waiting for verifier challenge; terminating"
            );
            return Err(());
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
) -> Result<(), ()> {
    let next_block = *starting_block.read().unwrap();
    tracing::info!(next_block, "requesting block replays from main node");
    let max_blocks_per_message = P::VERSION
        .supports_message(ZksMessageId::VerifierRoleRequest)
        .then_some(MAX_BLOCKS_PER_MESSAGE);
    let msg =
        ZksMessage::<P>::get_block_replays(next_block, max_blocks_per_message, record_overrides);
    outbound_tx.send(msg.encoded()).await.map_err(|_| ())
}

async fn receive_replay_and_verification<P: ZksProtocolVersionSpec>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    starting_block: Arc<RwLock<BlockNumber>>,
    replay_sender: mpsc::Sender<ReplayRecord>,
    peer_id: PeerId,
    verifier: Option<ExternalNodeVerifierConfig>,
) {
    let mut outgoing_verify_results = verifier
        .as_ref()
        .map(|verifier| verifier.outgoing_verify_results.subscribe());
    loop {
        tokio::select! {
            msg = conn.next() => {
                let Some(msg) = msg else {
                    break;
                };
                match msg {
                    ZksMessage::GetBlockReplays(_) => {
                        tracing::info!("ignoring request as local node is also waiting for records");
                    }
                    ZksMessage::VerifyBatch(request) => {
                        let Some(verifier) = &verifier else {
                            tracing::info!("ignoring verify batch request; verifier transport not configured");
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
                            break;
                        }
                    }
                    ZksMessage::BlockReplays(response) => {
                        for record in response.records {
                            let block_number = record.block_number();
                            tracing::debug!(block_number, "received block replay");
                            let record: ReplayRecord = match record.try_into() {
                                Ok(record) => record,
                                Err(error) => {
                                    tracing::info!(%error, "failed to recover replay block");
                                    return;
                                }
                            };

                            let expected_next_block = *starting_block.read().unwrap();
                            assert_eq!(block_number, expected_next_block);

                            if replay_sender.send(record).await.is_err() {
                                tracing::trace!("network replay channel is closed");
                                return;
                            }
                            *starting_block.write().unwrap() += 1;
                        }
                    }
                    other => {
                        tracing::info!(
                            ?other,
                            "ignoring unsupported message while waiting for replay"
                        );
                    }
                }
            }
            result = recv_outgoing_verify_result(&mut outgoing_verify_results) => {
                let Some(result) = result else {
                    continue;
                };
                if result.peer_id != peer_id {
                    continue;
                }
                if outbound_tx
                    .send(ZksMessage::<P>::VerifyBatchResult(result.message).encoded())
                    .await
                    .is_err()
                {
                    break;
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
