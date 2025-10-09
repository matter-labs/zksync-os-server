use tokio::sync::watch;
use tokio::sync::watch::Receiver;
use zksync_os_storage_api::{FinalityStatus, ReadFinality, WriteFinality};

#[derive(Debug, Clone)]
pub struct Finality {
    sender: watch::Sender<FinalityStatus>,
}

impl Finality {
    pub fn new(initial_status: FinalityStatus) -> Self {
        let (sender, _) = watch::channel(initial_status);
        Self { sender }
    }
}

impl ReadFinality for Finality {
    fn get_finality_status(&self) -> FinalityStatus {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> Receiver<FinalityStatus> {
        self.sender.subscribe()
    }
}

impl WriteFinality for Finality {
    fn update_finality_status(&self, f: impl FnOnce(&mut FinalityStatus)) {
        self.sender.send_modify(f);
    }
}
