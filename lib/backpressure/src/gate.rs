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
    pub fn is_open(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn wait_until_open(&mut self) {
        let _ = self.rx.wait_for(|open| *open).await;
    }
}
