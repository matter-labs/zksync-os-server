use alloy::primitives::U256;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_types::{SystemTxEnvelope, SystemTxType, ZkTransaction};

#[derive(Clone)]
pub struct InteropFeeSubpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    sender: Option<mpsc::Sender<U256>>,
    pending_fee: Option<U256>,
    next_interop_fee_number: u64,
}

impl InteropFeeSubpool {
    pub fn new(next_interop_fee_number: u64) -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: None,
                pending_fee: None,
                next_interop_fee_number,
            })),
        }
    }

    pub async fn best_transactions_stream(&self) -> InteropFeeTransactionsStream {
        let (sender, receiver) = mpsc::channel(1);
        let mut inner = self.inner.write().await;
        let next_interop_fee_number = inner.next_interop_fee_number;
        inner.sender = Some(sender);
        let state = if let Some(pending_fee) = inner.pending_fee {
            StreamState::Pending(SystemTxEnvelope::set_interop_fee(
                pending_fee,
                next_interop_fee_number,
            ))
        } else {
            StreamState::Empty(ReceiverStream::new(receiver), next_interop_fee_number)
        };
        InteropFeeTransactionsStream { state }
    }

    pub async fn insert(&self, interop_fee: U256) {
        let mut inner = self.inner.write().await;
        if let Some(sender) = &inner.sender
            && sender.send(interop_fee).await.is_err()
        {
            inner.sender.take();
        }
        inner.pending_fee = Some(interop_fee);
        self.notify.notify_waiters();
    }

    async fn consume_expected_tx(&self, tx: &SystemTxEnvelope) -> u64 {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().await;
                if let Some(pending_fee) = inner.pending_fee {
                    let expected_number = inner.next_interop_fee_number;
                    let expected_tx =
                        SystemTxEnvelope::set_interop_fee(pending_fee, expected_number);
                    assert_eq!(tx, &expected_tx);
                    inner.pending_fee.take();
                    inner.next_interop_fee_number += 1;
                    return expected_number;
                }
            }
            notified.await;
        }
    }

    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
        strict_subpool_cleanup: bool,
    ) -> Option<u64> {
        if !strict_subpool_cleanup {
            let last_interop_fee_number = txs.into_iter().map(|tx| match tx.system_subtype() {
                SystemTxType::SetInteropFee(interop_fee_number) => *interop_fee_number,
                other => panic!(
                    "tried to process unrelated system tx ({other:?}) in `InteropFeeSubpool`"
                ),
            });
            let last_interop_fee_number = last_interop_fee_number.last();
            if let Some(last_interop_fee_number) = last_interop_fee_number {
                let mut inner = self.inner.write().await;
                inner.next_interop_fee_number = inner
                    .next_interop_fee_number
                    .max(last_interop_fee_number + 1);
            }
            return last_interop_fee_number;
        }

        let mut last_interop_fee_number = None;
        for tx in txs {
            last_interop_fee_number = Some(self.consume_expected_tx(tx).await);
        }
        last_interop_fee_number
    }
}

pub struct InteropFeeTransactionsStream {
    state: StreamState,
}

enum StreamState {
    Empty(ReceiverStream<U256>, u64),
    Pending(SystemTxEnvelope),
    Closed,
}

impl Stream for InteropFeeTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &mut this.state {
            StreamState::Empty(receiver, next_interop_fee_number) => {
                let Some(interop_fee) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("interop fee updater stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                let next_interop_fee_number = *next_interop_fee_number;
                this.state = StreamState::Closed;
                Poll::Ready(Some(
                    SystemTxEnvelope::set_interop_fee(interop_fee, next_interop_fee_number).into(),
                ))
            }
            StreamState::Pending(tx) => {
                let tx = tx.clone();
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx.into()))
            }
            StreamState::Closed => Poll::Ready(None),
        }
    }
}
