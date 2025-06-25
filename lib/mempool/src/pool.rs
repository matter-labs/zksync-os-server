use crate::TransactionPool;
use crossbeam_queue::SegQueue;
use futures::stream::FusedStream;
use futures::task::AtomicWaker;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use vise::{Buckets, Counter, Histogram, LabeledFamily, Metrics};
use zksync_types::Transaction;

/// Thread-safe FIFO mempool that can be polled as a `Stream`.
///
/// Doesn't respect nonces
#[derive(Clone, Debug)]
pub struct Mempool {
    queue: Arc<SegQueue<Transaction>>,
    waker: Arc<AtomicWaker>, // wakes one waiting consumer
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "mempool")]
pub struct MempoolMetrics {
    #[metrics(labels = ["result"])]
    pub try_pop: LabeledFamily<&'static str, Counter<u64>>,
    #[metrics(buckets = Buckets::exponential(1.0..=1000000.0, 10.0))]
    pub len: Histogram<u64>,
}
#[vise::register]
pub(crate) static MEMPOOL_METRICS: vise::Global<MempoolMetrics> = vise::Global::new();

impl Mempool {
    pub fn new(forced_tx: Transaction) -> Self {
        let q = Arc::new(SegQueue::new());
        q.push(forced_tx);
        Self {
            queue: q,
            waker: Arc::new(AtomicWaker::new()),
        }
    }

    /* -------- producers ------------------------------------------- */

    pub fn insert(&self, tx: Transaction) {
        self.queue.push(tx);
        self.waker.wake(); // notify a waiting task
    }

    pub fn try_pop(&self) -> Option<Transaction> {
        let r = self.queue.pop();
        let metrics_key = if r.is_some() { "some" } else { "none" };
        MEMPOOL_METRICS.try_pop[&metrics_key].inc();
        MEMPOOL_METRICS.len.observe(self.queue.len() as u64);
        r
    }

    // /* -------- consumer helpers ------------------------------------ */
    //
    // pub async fn next_tx(&self) -> Transaction {
    //     loop {
    //         if let Some(tx) = self.try_pop() {
    //             return tx;
    //         }
    //         std::future::poll_fn(|cx| {
    //             self.waker.register(cx.waker());
    //             if let Some(tx) = self.try_pop() {
    //                 Poll::Ready(tx)
    //             } else {
    //                 Poll::Pending
    //             }
    //         })
    //         .await;
    //     }
    // }
}

/* -------- Stream / FusedStream for &Mempool ---------------------- */

impl Stream for Mempool {
    type Item = Transaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `self` is a pinned &mut Mempool; we can safely get a shared ref.
        let me: &Mempool = &self;

        if let Some(tx) = me.try_pop() {
            return Poll::Ready(Some(tx));
        }

        me.waker.register(cx.waker());

        // race-check in case a tx arrived after register()
        if let Some(tx) = me.try_pop() {
            me.waker.take();
            Poll::Ready(Some(tx))
        } else {
            Poll::Pending
        }
    }
}

impl FusedStream for Mempool {
    fn is_terminated(&self) -> bool {
        false
    } // endless stream
}

impl TransactionPool for Mempool {
    fn dyn_clone(&self) -> Box<dyn TransactionPool> {
        Box::new(self.clone())
    }

    fn add_transaction(&self, transaction: Transaction) {
        self.insert(transaction);
    }
}
