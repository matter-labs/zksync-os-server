use anyhow::Context;
use tokio::sync::watch;

/// Admission gate for internal pipeline sources.
///
/// This is deliberately separate from RPC transaction acceptance. `true` means
/// the block pipeline can accept more work; `false` means command sources should
/// stop forwarding new work until downstream lag clears.
#[derive(Debug, Clone)]
pub struct PipelineAdmissionGate {
    tx: watch::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct PipelineAdmissionReceiver {
    rx: watch::Receiver<bool>,
}

impl PipelineAdmissionGate {
    pub fn open() -> (Self, PipelineAdmissionReceiver) {
        let (tx, rx) = watch::channel(true);
        (Self { tx }, PipelineAdmissionReceiver { rx })
    }

    pub fn subscribe(&self) -> PipelineAdmissionReceiver {
        PipelineAdmissionReceiver {
            rx: self.tx.subscribe(),
        }
    }

    pub fn set_open(&self) {
        self.set(true);
    }

    pub fn set_closed(&self) {
        self.set(false);
    }

    pub fn set(&self, open: bool) {
        let _ = self.tx.send_if_modified(|current| {
            if *current == open {
                return false;
            }
            *current = open;
            true
        });
    }
}

impl PipelineAdmissionReceiver {
    pub fn is_open(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn wait_until_open(&mut self) -> anyhow::Result<()> {
        self.rx
            .wait_for(|open| *open)
            .await
            .map(|_| ())
            .context("pipeline admission gate sender dropped before reopening")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn receiver_waits_until_gate_opens() {
        let (gate, mut rx) = PipelineAdmissionGate::open();
        gate.set_closed();

        assert!(!rx.is_open());
        assert!(
            timeout(Duration::from_millis(20), rx.wait_until_open())
                .await
                .is_err()
        );

        gate.set_open();
        timeout(Duration::from_millis(100), rx.wait_until_open())
            .await
            .expect("gate opening should release waiter")
            .expect("open gate should not error");
        assert!(rx.is_open());
    }

    #[tokio::test]
    async fn receiver_errors_when_gate_sender_drops_while_closed() {
        let (gate, mut rx) = PipelineAdmissionGate::open();
        gate.set_closed();
        drop(gate);

        let result = timeout(Duration::from_millis(100), rx.wait_until_open())
            .await
            .expect("dropped sender should release waiter");
        assert!(result.is_err());
    }
}
