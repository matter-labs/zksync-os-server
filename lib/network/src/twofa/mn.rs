use super::config::MainNode2faConfig;
use super::wire::Zks2faMessage;
use crate::protocol::ProtocolEvent;
use crate::service::PeerVerifyBatchResult;
use crate::wire::auth::recover_verifier_signer;
use alloy::primitives::B256;
use alloy::primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_network_peers::PeerId;
use tokio::sync::mpsc;

/// Background task that drives the main-node side of a `zks_2fa` connection.
///
/// Authenticates a verifier external node (role request -> challenge -> auth), then forwards any
/// [`VerifyBatchResult`](crate::wire::verification::VerifyBatchResult) the peer returns into the
/// node via `verify_result_tx`. Outbound `VerifyBatch` requests are pushed onto this connection by
/// the verify dispatcher via the connection registry, not from this task.
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
    } = config;
    let mut pending_verifier_nonce: Option<B256> = None;
    loop {
        match conn.next().await {
            Some(Zks2faMessage::VerifierRoleRequest(_)) => {
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
            Some(Zks2faMessage::VerifierAuth(auth)) => {
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
            Some(Zks2faMessage::VerifyBatchResult(result)) => {
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
                tracing::info!(
                    ?msg,
                    "received unexpected zks_2fa message from peer; terminating"
                );
                return;
            }
            None => return,
        }
    }
}
