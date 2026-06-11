use tokio::sync::watch;

/// Admission gate for internal pipeline sources.
///
/// Driven by the same backpressure condition as RPC transaction acceptance
/// (the monitor closes the gate exactly when it suspends acceptance), but
/// delivered over a separate channel so command sources don't depend on the
/// RPC-facing state type. `true` means the block pipeline can accept more
/// work; `false` means command sources should stop forwarding new work until
/// downstream lag clears.
#[derive(Debug)]
pub struct PipelineAdmissionGate {
    tx: watch::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct PipelineAdmissionReceiver {
    rx: watch::Receiver<bool>,
}

impl Default for PipelineAdmissionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineAdmissionGate {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(true);
        Self { tx }
    }

    pub fn subscribe(&self) -> PipelineAdmissionReceiver {
        PipelineAdmissionReceiver {
            rx: self.tx.subscribe(),
        }
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
    /// Returns the last gate state set by the monitor. If the monitor has
    /// stopped (sender dropped), keeps reporting the last state it set.
    pub fn is_open(&self) -> bool {
        *self.rx.borrow()
    }

    /// Waits until the gate is open.
    ///
    /// If the gate sender is dropped while the gate is closed, this future
    /// never resolves: the gate freezes in its last state, consistent with
    /// [`Self::is_open`]. The sender is owned by the backpressure monitor,
    /// a critical task that only exits on node shutdown, so a frozen-closed
    /// gate just parks the source until the pipeline is torn down.
    pub async fn wait_until_open(&mut self) {
        if self.rx.wait_for(|open| *open).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn gate_starts_open() {
        let gate = PipelineAdmissionGate::new();
        let mut rx = gate.subscribe();
        assert!(rx.is_open());
        timeout(Duration::from_secs(1), rx.wait_until_open())
            .await
            .expect("wait_until_open must resolve immediately on an open gate");
    }

    #[tokio::test]
    async fn wait_blocks_while_closed_and_resolves_on_open() {
        let gate = PipelineAdmissionGate::new();
        let mut rx = gate.subscribe();

        gate.set(false);
        assert!(!rx.is_open());
        assert!(
            timeout(Duration::from_millis(50), rx.wait_until_open())
                .await
                .is_err(),
            "wait_until_open must not resolve while the gate is closed"
        );

        let wait = tokio::spawn(async move {
            rx.wait_until_open().await;
            rx
        });
        gate.set(true);
        let rx = timeout(Duration::from_secs(1), wait)
            .await
            .expect("wait_until_open must resolve once the gate opens")
            .unwrap();
        assert!(rx.is_open());
    }

    #[tokio::test]
    async fn gate_freezes_in_last_state_when_sender_dropped() {
        // Dropped while closed: stays closed, wait_until_open never resolves.
        let gate = PipelineAdmissionGate::new();
        let mut rx = gate.subscribe();
        gate.set(false);
        drop(gate);
        assert!(!rx.is_open());
        assert!(
            timeout(Duration::from_millis(50), rx.wait_until_open())
                .await
                .is_err(),
            "wait_until_open must not resolve after the sender is dropped while closed"
        );

        // Dropped while open: stays open, wait_until_open resolves.
        let gate = PipelineAdmissionGate::new();
        let mut rx = gate.subscribe();
        drop(gate);
        assert!(rx.is_open());
        timeout(Duration::from_secs(1), rx.wait_until_open())
            .await
            .expect("wait_until_open must resolve after the sender is dropped while open");
    }
}
