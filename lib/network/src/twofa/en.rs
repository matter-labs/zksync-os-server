use super::config::ExternalNode2faConfig;
use super::wire::Zks2faMessage;
use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::wire::auth::{VerifierAuth, verifier_auth_prehash};
use alloy::primitives::bytes::BytesMut;
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use secrecy::ExposeSecret;
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};

/// Background task that drives the external-node side of a `zks_2fa` connection.
///
/// Performs the verifier handshake immediately, then forwards each received `VerifyBatch` request
/// into the local verifier via `verify_batch_tx` and relays signed `VerifyBatchResult`s back to the
/// requesting main node.
pub(super) async fn run_2fa_en_connection(
    mut conn: impl Stream<Item = Zks2faMessage> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    peer_id: PeerId,
    config: ExternalNode2faConfig,
) {
    // Subscribe before the handshake so a result broadcast right after authentication is not
    // missed (a `broadcast::Receiver` only observes messages sent after it subscribed).
    let outgoing_verify_results = config.outgoing_verify_results.subscribe();
    if perform_verifier_handshake(&mut conn, &outbound_tx, &config)
        .await
        .is_err()
    {
        return;
    }
    receive_verification(conn, outbound_tx, peer_id, config, outgoing_verify_results).await;
}

async fn perform_verifier_handshake(
    conn: &mut (impl Stream<Item = Zks2faMessage> + Unpin),
    outbound_tx: &mpsc::Sender<BytesMut>,
    config: &ExternalNode2faConfig,
) -> Result<(), ()> {
    if outbound_tx
        .send(Zks2faMessage::verifier_role_request().encoded())
        .await
        .is_err()
    {
        return Err(());
    }

    let signer = match PrivateKeySigner::from_str(config.signing_key.expose_secret()) {
        Ok(signer) => signer,
        Err(error) => {
            tracing::info!(%error, "invalid verifier signing key; terminating");
            return Err(());
        }
    };

    let challenge = match conn.next().await {
        Some(Zks2faMessage::VerifierChallenge(challenge)) => challenge,
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

    let msg = Zks2faMessage::VerifierAuth(VerifierAuth {
        signature: signature.as_bytes().to_vec().into(),
    });
    if outbound_tx.send(msg.encoded()).await.is_err() {
        return Err(());
    }
    Ok(())
}

async fn receive_verification(
    mut conn: impl Stream<Item = Zks2faMessage> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    peer_id: PeerId,
    config: ExternalNode2faConfig,
    mut outgoing_verify_results: broadcast::Receiver<PeerVerifyBatchResult>,
) {
    loop {
        tokio::select! {
            msg = conn.next() => {
                let Some(msg) = msg else {
                    break;
                };
                match msg {
                    Zks2faMessage::VerifyBatch(request) => {
                        if config
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
                    other => {
                        tracing::info!(?other, "ignoring unexpected zks_2fa message");
                    }
                }
            }
            result = recv_outgoing_verify_result(&mut outgoing_verify_results) => {
                let Some(result) = result else {
                    break;
                };
                if result.peer_id != peer_id {
                    continue;
                }
                if outbound_tx
                    .send(Zks2faMessage::VerifyBatchResult(result.message).encoded())
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
    receiver: &mut broadcast::Receiver<PeerVerifyBatchResult>,
) -> Option<PeerVerifyBatchResult> {
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
