use crate::InteropTxPool;
use crate::subpools::l1::L1Subpool;
use crate::subpools::l2::L2Subpool;
use alloy::primitives::B256;
use futures::StreamExt;
use futures::stream::{BoxStream, PollNext};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zksync_os_types::{ProtocolSemanticVersion, UpgradeTransaction, ZkTransaction};

pub struct Pool<T: L2Subpool> {
    upgrade_transactions: mpsc::Receiver<UpgradeTransaction>,
    // todo: rename to `InteropSubpool` and move to `subpools`
    interop_subpool: InteropTxPool,
    l1_subpool: L1Subpool,
    l2_subpool: T,
    // todo: should be a part of `InteropTxPool`
    interop_roots_per_tx: usize,
}

impl<T: L2Subpool> Pool<T> {
    pub fn new(
        upgrade_transactions: mpsc::Receiver<UpgradeTransaction>,
        interop_subpool: InteropTxPool,
        l1_subpool: L1Subpool,
        l2_subpool: T,
        interop_roots_per_tx: usize,
    ) -> Self {
        Self {
            upgrade_transactions,
            interop_subpool,
            l1_subpool,
            l2_subpool,
            interop_roots_per_tx,
        }
    }

    pub async fn best_transactions_stream<'a>(
        &'a mut self,
        next_interop_tx_allowed_after: Instant,
    ) -> TransactionsStream<'a> {
        let interop_stream = self.interop_subpool.interop_transactions_with_delay(
            self.interop_roots_per_tx,
            next_interop_tx_allowed_after,
        );
        let mut interop_stream = tokio_stream::StreamExt::peekable(interop_stream);

        let l1_stream = self.l1_subpool.best_transactions_stream();
        let l2_stream = self.l2_subpool.best_transactions_stream();
        fn prio_left(_: &mut ()) -> PollNext {
            PollNext::Left
        }
        let l1_l2_stream = futures::stream::select_with_strategy(l1_stream, l2_stream, prio_left);
        let mut l1_l2_stream = tokio_stream::StreamExt::peekable(l1_l2_stream);

        tokio::select! {
            // If you run this example without `biased;`, the polling order is
            // pseudo-random, and the assertions on the value of count will
            // (probably) fail.
            biased;

            Some(upgrade_tx) = self.upgrade_transactions.recv() => {
                TransactionsStream::upgrade(upgrade_tx)
            }
            Some(_) = interop_stream.peek() => {
                TransactionsStream {
                    upgrade_info: None,
                    stream: interop_stream.map(ZkTransaction::from).boxed(),
                }
            }
            Some(_) = l1_l2_stream.peek() => {
                TransactionsStream {
                    upgrade_info: None,
                    stream: l1_l2_stream.boxed(),
                }
            }

            else => {
                todo!()
            }
        }
    }
}

// todo: move to `types`
pub struct UpgradeInfo {
    /// Instruction for the sequencer to NOT execute the upgrade transaction
    /// until the given timestamp.
    /// Represents a timestamp in seconds since UNIX_EPOCH
    pub timestamp: u64,
    /// Which protocol version will be used after the upgrade transaction is executed.
    pub protocol_version: ProtocolSemanticVersion,
    /// Preimages (e.g. force deployments) for the upgrade transaction (if any).
    pub force_preimages: Vec<(B256, Vec<u8>)>,
}

pub struct TransactionsStream<'a> {
    pub upgrade_info: Option<UpgradeInfo>,
    pub stream: BoxStream<'a, ZkTransaction>,
}

impl TransactionsStream<'_> {
    fn upgrade(upgrade_tx: UpgradeTransaction) -> Self {
        let upgrade_info = Some(UpgradeInfo {
            timestamp: upgrade_tx.timestamp,
            protocol_version: upgrade_tx.protocol_version,
            force_preimages: upgrade_tx.force_preimages,
        });
        // todo: rename `.tx` to `.envelope`
        if let Some(envelope) = upgrade_tx.tx {
            TransactionsStream {
                upgrade_info,
                stream: futures::stream::iter(vec![envelope.into()]).boxed(),
            }
        } else {
            // fixme: this is different from old impl, is it okay to return empty iterator here?
            TransactionsStream {
                upgrade_info,
                stream: futures::stream::empty().boxed(),
            }
        }
    }
}
