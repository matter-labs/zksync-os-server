use super::config::MainNode2faConfig;
use super::wire::Zks2faMessage;
use crate::protocol::ProtocolEvent;
use crate::service::{PeerVerifyBatch, PeerVerifyBatchResult};
use crate::wire::auth::{VerifierAuth, recover_verifier_signer, verifier_auth_prehash};
use alloy::primitives::B256;
use alloy::primitives::bytes::BytesMut;
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use secrecy::ExposeSecret;
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};

/// Background task that drives the main-node side of a `zks_2fa` connection.
///
/// Authenticates a verifier peer (role request -> challenge -> auth), then forwards any
/// [`VerifyBatchResult`](crate::wire::verification::VerifyBatchResult) the peer returns into the
/// node via `verify_result_tx`. Outbound `VerifyBatch` requests are pushed onto this connection by
/// the verify dispatcher via the connection registry, not from this task.
///
/// When this node itself carries a verifier attachment (a committee validator acting as a batch
/// verifier), it *also* requests the verifier role from the peer and answers incoming
/// `VerifyBatch` requests — so two validators verify for each other over one session regardless
/// of who dialed whom. The legacy in-`zks` handler (`crate::protocol::mn`) carries the same
/// symmetric behavior for pre-`zks/5` peers and goes away with them.
pub(super) async fn run_2fa_mn_connection(
    mut conn: impl Stream<Item = Zks2faMessage> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    peer_id: PeerId,
    config: MainNode2faConfig,
) {
    let MainNode2faConfig {
        accepted_verifier_signers,
        verify_result_tx,
        verification,
    } = config;

    // Our own verifier half, when attached: request the role up front; the
    // signer answers the peer's challenge below. Subscribe to results before the
    // request so a broadcast right after authentication is not missed.
    let mut outgoing_verify_results = verification
        .as_ref()
        .map(|verifier| verifier.outgoing_verify_results.subscribe());
    let our_signer = verification.as_ref().and_then(|verifier| {
        match PrivateKeySigner::from_str(verifier.signing_key.expose_secret()) {
            Ok(signer) => Some(signer),
            Err(error) => {
                tracing::info!(%error, "invalid verifier signing key; not requesting verifier role");
                None
            }
        }
    });
    if our_signer.is_some() {
        let msg = Zks2faMessage::verifier_role_request();
        if outbound_tx.send(msg.encoded()).await.is_err() {
            return;
        }
    }

    // Challenge we issued to the peer (its role request), awaiting its auth.
    let mut pending_verifier_nonce: Option<B256> = None;
    loop {
        tokio::select! {
            msg = conn.next() => {
                let Some(msg) = msg else {
                    return;
                };
                match msg {
                    Zks2faMessage::VerifierRoleRequest(_) => {
                        events_sender
                            .send(ProtocolEvent::VerifierRoleRequested { peer_id })
                            .ok();
                        let nonce = B256::random();
                        if outbound_tx
                            .send(Zks2faMessage::verifier_challenge(nonce).encoded())
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
                    Zks2faMessage::VerifierAuth(auth) => {
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
                    Zks2faMessage::VerifierChallenge(challenge) => {
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
                        let msg = Zks2faMessage::VerifierAuth(VerifierAuth {
                            signature: signature.as_bytes().to_vec().into(),
                        });
                        if outbound_tx.send(msg.encoded()).await.is_err() {
                            return;
                        }
                    }
                    // The peer (a settler) asks us to verify one of its batches.
                    Zks2faMessage::VerifyBatch(request) => {
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
                    Zks2faMessage::VerifyBatchResult(result) => {
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
                if outbound_tx
                    .send(Zks2faMessage::VerifyBatchResult(result.message).encoded())
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
