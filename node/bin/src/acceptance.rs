use futures::stream::{StreamExt, select_all};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

pub struct TxAcceptanceGate {
    receivers: Vec<watch::Receiver<TransactionAcceptanceState>>,
    tx: watch::Sender<TransactionAcceptanceState>,
}

impl TxAcceptanceGate {
    pub fn new() -> (Self, watch::Receiver<TransactionAcceptanceState>) {
        let (tx, rx) = watch::channel(TransactionAcceptanceState::Accepting);
        (
            Self {
                receivers: vec![],
                tx,
            },
            rx,
        )
    }

    pub fn register(&mut self, rx: watch::Receiver<TransactionAcceptanceState>) {
        self.receivers.push(rx);
    }

    pub async fn run(self, mut stop_receiver: watch::Receiver<bool>) {
        if *stop_receiver.borrow_and_update() {
            return;
        }

        let streams = self
            .receivers
            .iter()
            .map(|rx| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();

        if streams.is_empty() {
            return;
        }

        self.evaluate_and_send();

        let mut combined = select_all(streams);
        loop {
            tokio::select! {
                Some(_) = combined.next() => self.evaluate_and_send(),
                _ = stop_receiver.changed() => return,
                else => return,
            }
        }
    }

    fn evaluate_and_send(&self) {
        let reasons: Vec<NotAcceptingReason> = self
            .receivers
            .iter()
            .flat_map(|rx| match rx.borrow().clone() {
                TransactionAcceptanceState::NotAccepting(reasons) => reasons,
                TransactionAcceptanceState::Accepting => vec![],
            })
            .collect();

        let new_state = if reasons.is_empty() {
            TransactionAcceptanceState::Accepting
        } else {
            TransactionAcceptanceState::NotAccepting(reasons)
        };

        self.tx.send_if_modified(|current| {
            if *current == new_state {
                return false;
            }
            *current = new_state.clone();
            true
        });
    }
}
