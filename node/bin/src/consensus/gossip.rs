//! Transaction gossip on the committee network: a transaction reaches the next
//! leader no matter which validator's RPC received it.

use alloy::consensus::transaction::SignerRecoverable as _;
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use commonware_cryptography::ed25519;
use commonware_p2p::{Receiver, Sender};
use std::num::NonZeroU32;
use zksync_os_consensus_execution::metrics::CONSENSUS_METRICS;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_types::L2Envelope;

/// Most transactions one gossip message carries (the sender drains whatever is
/// immediately available up to this).
const MAX_TXS_PER_GOSSIP: usize = 64;

/// Starts both halves of committee transaction gossip on the consensus runtime.
///
/// Outbound: every transaction newly inserted into this node's L2 pool — whether it
/// arrived over RPC or from a peer — is offered to the whole committee once. The pool
/// does not announce transactions it already knows, which is what keeps the flood
/// from echoing forever while still letting any holder heal a lost delivery.
///
/// Inbound: gossiped transactions go through the same decoding and pool validation as
/// local RPC submissions; duplicates and invalid ones die in the pool. Peers are
/// authenticated committee members, so gossip adds no new spam surface beyond what
/// each validator's own RPC already accepts.
pub(super) fn start_tx_gossip<C, P, TxSender, TxReceiver>(
    context: &C,
    pool: P,
    sender: TxSender,
    mut receiver: TxReceiver,
    max_message_size: NonZeroU32,
    role: crate::config::ConsensusRole,
) where
    C: commonware_runtime::Spawner + commonware_runtime::Metrics,
    P: L2Subpool + Clone,
    TxSender: Sender<PublicKey = ed25519::PublicKey>,
    TxReceiver: Receiver<PublicKey = ed25519::PublicKey>,
{
    // Leave generous headroom under the network's message cap; a batch is cut early
    // when it grows past this.
    let byte_budget = max_message_size.get() as usize / 2;

    // Observers receive gossip (the channel is registered either way — an
    // unrecognized channel would get the *sender* banned) but do not broadcast:
    // their transactions travel to validators over RPC forwarding instead. Gossip
    // injection from observers is the ratified later step, not this one.
    if role.is_validator() {
        let gossip_pool = pool.clone();
        start_tx_gossip_out(context, gossip_pool, sender, byte_budget);
    }

    context
        .child("tx_gossip_in")
        .spawn(move |task_context| async move {
            // `recv` errors once the network tears down, but watch the stop signal
            // too so this task never outlives the runtime with its pool handle.
            let mut stopped = task_context.stopped();
            loop {
                let (peer, message) = tokio::select! {
                    _ = &mut stopped => return,
                    received = receiver.recv() => match received {
                        Ok(received) => received,
                        Err(_) => return,
                    },
                };
                CONSENSUS_METRICS.tx_gossip[&"received"].inc();
                let Ok(batch) = <Vec<alloy::primitives::Bytes> as alloy_rlp::Decodable>::decode(
                    &mut message.as_ref(),
                ) else {
                    tracing::debug!(?peer, "undecodable transaction gossip; ignoring");
                    CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                    continue;
                };
                for tx_bytes in batch {
                    let Ok(envelope) = L2Envelope::decode_2718(&mut tx_bytes.as_ref()) else {
                        tracing::debug!(?peer, "undecodable gossiped transaction; ignoring");
                        CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                        continue;
                    };
                    let Ok(transaction) = envelope.try_into_recovered() else {
                        tracing::debug!(
                            ?peer,
                            "gossiped transaction with a bad signature; ignoring"
                        );
                        CONSENSUS_METRICS.tx_gossip[&"undecodable"].inc();
                        continue;
                    };
                    match pool.add_gossiped_transaction(transaction).await {
                        Ok(_) => {
                            CONSENSUS_METRICS.tx_gossip[&"admitted"].inc();
                        }
                        Err(error) => {
                            // Routine: the pool already knows most re-gossiped
                            // transactions.
                            tracing::debug!(%error, "gossiped transaction not admitted");
                            CONSENSUS_METRICS.tx_gossip[&"ignored"].inc();
                        }
                    }
                }
            }
        });
}

/// The outbound half of transaction gossip: drains the pool's new-transaction
/// stream into batched broadcasts. Validators only (see [`start_tx_gossip`]).
fn start_tx_gossip_out<C, P, TxSender>(
    context: &C,
    pool: P,
    mut sender: TxSender,
    byte_budget: usize,
) where
    C: commonware_runtime::Spawner + commonware_runtime::Metrics,
    P: L2Subpool + Clone,
    TxSender: Sender<PublicKey = ed25519::PublicKey>,
{
    context
        .child("tx_gossip_out")
        .spawn(move |task_context| async move {
            // The pool's listener never closes on consensus shutdown (the pool lives
            // node-side), so this task must watch the stop signal itself — a parked
            // task would hold pool handles (and the databases under them) past the
            // runtime's shutdown deadline.
            let mut stopped = task_context.stopped();
            let mut new_txs = pool.new_transactions_listener();
            loop {
                let event = tokio::select! {
                    _ = &mut stopped => return,
                    event = new_txs.recv() => match event {
                        Some(event) => event,
                        None => return,
                    },
                };
                // Greedily drain whatever else is already queued into one message.
                let mut batch = vec![encode_gossiped_tx(&event)];
                let mut batch_bytes = batch[0].len();
                while batch.len() < MAX_TXS_PER_GOSSIP && batch_bytes < byte_budget {
                    match new_txs.try_recv() {
                        Ok(event) => {
                            let encoded = encode_gossiped_tx(&event);
                            batch_bytes += encoded.len();
                            batch.push(encoded);
                        }
                        Err(_) => break,
                    }
                }
                let message = alloy_rlp::encode(&batch);
                // `send` is synchronous and returns the delivery list; gossip is
                // best-effort, so an empty delivery is not an error (network teardown
                // is caught by the stop signal above).
                let _ = sender.send(commonware_p2p::Recipients::All, message, false);
            }
        });
}

/// The canonical wire form of one gossiped transaction: its EIP-2718 encoding — the
/// exact bytes a user would submit over RPC.
fn encode_gossiped_tx(
    event: &zksync_os_mempool::NewTransactionEvent<zksync_os_mempool::L2PooledTransaction>,
) -> alloy::primitives::Bytes {
    let (envelope, _signer) = event.transaction.to_consensus().into_parts();
    envelope.encoded_2718().into()
}
