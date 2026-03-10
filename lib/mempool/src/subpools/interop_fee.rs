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
    sender: Option<mpsc::Sender<SystemTxEnvelope>>,
    pending_tx: Option<SystemTxEnvelope>,
}

impl Default for InteropFeeSubpool {
    fn default() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: None,
                pending_tx: None,
            })),
        }
    }
}

impl InteropFeeSubpool {
    pub async fn best_transactions_stream(&self) -> InteropFeeTransactionsStream {
        let (sender, receiver) = mpsc::channel(1);
        let mut inner = self.inner.write().await;
        inner.sender = Some(sender);
        let state = if let Some(pending_tx) = inner.pending_tx.clone() {
            StreamState::Pending(pending_tx)
        } else {
            StreamState::Empty(ReceiverStream::new(receiver))
        };
        InteropFeeTransactionsStream { state }
    }

    pub async fn insert(&self, tx: SystemTxEnvelope) {
        assert_eq!(
            tx.system_subtype(),
            &SystemTxType::SetInteropFee,
            "tried to insert unrelated system tx ({:?}) into `InteropFeeSubpool`",
            tx.system_subtype()
        );
        let mut inner = self.inner.write().await;
        if let Some(sender) = &inner.sender
            && sender.send(tx.clone()).await.is_err()
        {
            inner.sender.take();
        }
        inner.pending_tx = Some(tx);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> SystemTxEnvelope {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().await;
                if let Some(pending_tx) = inner.pending_tx.take() {
                    return pending_tx;
                }
            }
            notified.await;
        }
    }

    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
        strict_subpool_cleanup: bool,
    ) {
        if !strict_subpool_cleanup {
            return;
        }

        for tx in txs {
            let pending_tx = self.pop_wait().await;
            assert_eq!(tx, &pending_tx);
        }
    }
}

pub struct InteropFeeTransactionsStream {
    state: StreamState,
}

enum StreamState {
    Empty(ReceiverStream<SystemTxEnvelope>),
    Pending(SystemTxEnvelope),
    Closed,
}

impl Stream for InteropFeeTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &mut this.state {
            StreamState::Empty(receiver) => {
                let Some(tx) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("interop fee updater stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx.into()))
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
