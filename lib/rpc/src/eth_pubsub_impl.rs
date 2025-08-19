use crate::eth_impl::build_api_log;
use crate::rpc_storage::ReadRpcStorage;
use alloy::consensus::Sealed;
use alloy::consensus::transaction::TransactionInfo;
use alloy::primitives::{TxHash, U256};
use alloy::rpc::types::pubsub::{Params, SubscriptionKind};
use alloy::rpc::types::{Filter, Log, Transaction};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use jsonrpsee::{PendingSubscriptionSink, SubscriptionMessage, SubscriptionSink};
use serde::Serialize;
use std::ops::Deref;
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_mempool::{EthPooledTransaction, L2TransactionPool, NewTransactionEvent};
use zksync_os_rpc_api::pubsub::EthPubSubApiServer;
use zksync_os_types::BlockExt;

#[derive(Clone)]
pub struct EthPubsubNamespace<RpcStorage, Mempool> {
    storage: RpcStorage,
    mempool: Mempool,
}

impl<RpcStorage, Mempool> EthPubsubNamespace<RpcStorage, Mempool> {
    pub fn new(storage: RpcStorage, mempool: Mempool) -> Self {
        Self { storage, mempool }
    }
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool>
    EthPubsubNamespace<RpcStorage, Mempool>
{
    /// Returns a stream that yields all new RPC blocks.
    fn new_headers_stream(
        &self,
    ) -> impl Stream<Item = alloy::rpc::types::Header> + use<RpcStorage, Mempool> {
        self.storage
            .block_subscriptions()
            .block_stream()
            .map(|notification| {
                alloy::rpc::types::Header::from_consensus(
                    Sealed::new_unchecked(
                        notification.block.as_ref().header.clone(),
                        notification.block.hash(),
                    ),
                    None,
                    Some(U256::from(notification.block.as_ref().deref().rlp_length())),
                )
            })
    }

    /// Returns a stream that yields all logs that match the given filter.
    fn log_stream(&self, filter: Filter) -> impl Stream<Item = Log> + use<RpcStorage, Mempool> {
        self.storage
            .block_subscriptions()
            .block_stream()
            .flat_map(move |notification| {
                let mut logs = Vec::new();
                for (tx_hash, stored_tx) in notification.transactions.iter() {
                    for (i, log) in stored_tx.receipt.logs().iter().enumerate() {
                        if filter.matches(log) {
                            logs.push(build_api_log(
                                *tx_hash,
                                log.clone(),
                                stored_tx.meta.clone(),
                                i as u64,
                            ));
                        }
                    }
                }
                futures::stream::iter(logs)
            })
    }

    /// Returns a stream that yields all transaction hashes emitted by the mempool.
    fn pending_transaction_hashes_stream(
        &self,
    ) -> impl Stream<Item = TxHash> + use<RpcStorage, Mempool> {
        ReceiverStream::new(self.mempool.pending_transactions_listener())
    }

    /// Returns a stream that yields all transactions emitted by the mempool.
    fn full_pending_transaction_stream(
        &self,
    ) -> impl Stream<Item = NewTransactionEvent<EthPooledTransaction>> + use<RpcStorage, Mempool>
    {
        self.mempool.new_pending_pool_transactions_listener()
    }

    /// The actual handler for an accepted [`EthPubSub::subscribe`] call.
    pub async fn handle_accepted(
        &self,
        accepted_sink: &SubscriptionSink,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> EthPubsubResult<BoxStream<'static, SubscriptionMessage>> {
        match kind {
            SubscriptionKind::NewHeads => {
                if params.unwrap_or_default() != Params::None {
                    return Err(EthPubsubError::InvalidParamsForNewHeads);
                }
                Ok(serialize_stream_as_messages(
                    accepted_sink,
                    self.new_headers_stream(),
                ))
            }
            SubscriptionKind::Logs => {
                // if no params are provided, used default filter params
                let filter = match params.unwrap_or_default() {
                    Params::Logs(filter) => *filter,
                    Params::Bool(_) => return Err(EthPubsubError::InvalidBoolForLogs),
                    Params::None => Default::default(),
                };
                Ok(serialize_stream_as_messages(
                    accepted_sink,
                    self.log_stream(filter),
                ))
            }
            SubscriptionKind::NewPendingTransactions => {
                match params.unwrap_or_default() {
                    Params::Bool(true) => {
                        // full transaction objects requested
                        let stream = self.full_pending_transaction_stream().map(|event| {
                            Transaction::from_transaction(
                                event.transaction.to_consensus(),
                                TransactionInfo::default(),
                            )
                        });
                        Ok(serialize_stream_as_messages(accepted_sink, stream))
                    }
                    Params::Bool(false) | Params::None => {
                        // only hashes requested
                        Ok(serialize_stream_as_messages(
                            accepted_sink,
                            self.pending_transaction_hashes_stream(),
                        ))
                    }
                    Params::Logs(_) => Err(EthPubsubError::InvalidLogFilterForPendingTxs),
                }
            }
            SubscriptionKind::Syncing => Err(EthPubsubError::SyncingNotSupported),
        }
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> EthPubSubApiServer
    for EthPubsubNamespace<RpcStorage, Mempool>
{
    async fn subscribe(
        &self,
        subscription_sink: PendingSubscriptionSink,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = subscription_sink.accept().await?;
        let message_stream = self.handle_accepted(&sink, kind, params).await?;
        // todo: dangling task, respect stop_receiver and register task when there is a proper system
        tokio::spawn(async move {
            pipe_from_stream(sink, message_stream).await;
        });

        Ok(())
    }
}

/// Transform a stream of any serializable items into a stream of [`SubscriptionMessage`] meant to
/// be sent over JSON-RPC subscription connection. Erases the type of underlying item.
fn serialize_stream_as_messages<T, St>(
    sink: &SubscriptionSink,
    stream: St,
) -> BoxStream<'static, SubscriptionMessage>
where
    St: Stream<Item = T> + Unpin + Send + 'static,
    T: Serialize + Send + 'static,
{
    let method_name = sink.method_name().to_string();
    let subscription_id = sink.subscription_id();
    Box::pin(stream.filter_map(move |item| {
        let method_name = method_name.clone();
        let subscription_id = subscription_id.clone();
        async move {
            match SubscriptionMessage::new(&method_name, subscription_id, &item) {
                Ok(msg) => Some(msg),
                Err(err) => {
                    tracing::error!(?err, "failed to serialize subscription message");
                    None
                }
            }
        }
    }))
}

/// Pipes all stream messages to the subscription sink.
async fn pipe_from_stream(
    sink: SubscriptionSink,
    mut stream: impl Stream<Item = SubscriptionMessage> + Unpin,
) {
    loop {
        tokio::select! {
            _ = sink.closed() => {
                // connection dropped
                break;
            },
            maybe_item = stream.next() => {
                let msg = match maybe_item {
                    Some(msg) => msg,
                    None => {
                        // stream ended
                        break;
                    },
                };

                if sink.send(msg).await.is_err() {
                    // connection dropped
                    break;
                }
            }
        }
    }
}

type EthPubsubResult<T> = Result<T, EthPubsubError>;

/// Errors that can occur in the handler implementation
#[derive(Debug, thiserror::Error)]
pub enum EthPubsubError {
    // todo: allow subscription to sync status once we have consensus integrated
    #[error("syncing subscriptions are not supported yet")]
    SyncingNotSupported,
    #[error("invalid params: log filter specified for pending transaction subscription")]
    InvalidLogFilterForPendingTxs,
    #[error("invalid params: boolean (transaction fullness) specified for log subscription")]
    InvalidBoolForLogs,
    #[error("invalid params: specified for head subscription (expected none)")]
    InvalidParamsForNewHeads,
}
