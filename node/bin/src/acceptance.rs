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

    pub async fn run(self) {
        // Evaluate immediately so the initial state is correct before any changes arrive.
        self.evaluate_and_send();

        // Merge all receivers into a single stream. WatchStream::from_changes only fires
        // on subsequent changes, not the initial value — the evaluate_and_send() call above
        // handles the initial snapshot.
        let streams = self
            .receivers
            .iter()
            .map(|rx| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();

        if streams.is_empty() {
            return;
        }

        let mut combined = select_all(streams);
        while combined.next().await.is_some() {
            self.evaluate_and_send();
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

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};

    fn pipeline_backpressure_reason() -> NotAcceptingReason {
        NotAcceptingReason::PipelineBackpressure {
            causes: vec![BackpressureCause {
                component: "batcher",
                trigger: BackpressureTrigger::BlockLagTooHigh {
                    threshold: 100,
                    actual: 200,
                },
            }],
        }
    }

    #[tokio::test]
    async fn single_channel_not_accepting_propagates() {
        let (mut gate, gate_rx) = TxAcceptanceGate::new();
        let (tx, rx) = watch::channel(TransactionAcceptanceState::Accepting);
        gate.register(rx);

        tokio::spawn(gate.run());

        tx.send(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]))
        .unwrap();

        // Give the gate task time to react.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        assert!(matches!(
            *gate_rx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));
    }

    #[tokio::test]
    async fn both_channels_not_accepting_merges_all_reasons() {
        let (mut gate, gate_rx) = TxAcceptanceGate::new();
        let (tx1, rx1) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));
        let (tx2, rx2) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            pipeline_backpressure_reason(),
        ]));
        gate.register(rx1);
        gate_rx.borrow(); // keep alive
        gate.register(rx2);

        tokio::spawn(gate.run());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Trigger a change so the gate re-evaluates with both channels NotAccepting.
        tx1.send(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]))
        .unwrap();
        tx2.send(TransactionAcceptanceState::NotAccepting(vec![
            pipeline_backpressure_reason(),
        ]))
        .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let state = gate_rx.borrow().clone();
        if let TransactionAcceptanceState::NotAccepting(reasons) = state {
            assert_eq!(reasons.len(), 2);
        } else {
            panic!("expected NotAccepting with 2 reasons");
        }
    }

    #[tokio::test]
    async fn one_clears_other_remains_not_accepting() {
        let (mut gate, gate_rx) = TxAcceptanceGate::new();
        let (tx1, rx1) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));
        let (_tx2, rx2) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            pipeline_backpressure_reason(),
        ]));
        gate.register(rx1);
        gate.register(rx2);

        tokio::spawn(gate.run());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Channel 1 clears; channel 2 still NotAccepting.
        tx1.send(TransactionAcceptanceState::Accepting).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        assert!(matches!(
            *gate_rx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));
    }

    #[tokio::test]
    async fn both_clear_gate_emits_accepting() {
        let (mut gate, gate_rx) = TxAcceptanceGate::new();
        let (tx1, rx1) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));
        let (tx2, rx2) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            pipeline_backpressure_reason(),
        ]));
        gate.register(rx1);
        gate.register(rx2);

        tokio::spawn(gate.run());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        tx1.send(TransactionAcceptanceState::Accepting).unwrap();
        tx2.send(TransactionAcceptanceState::Accepting).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        assert_eq!(*gate_rx.borrow(), TransactionAcceptanceState::Accepting);
    }
}
