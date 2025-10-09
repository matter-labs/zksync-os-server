use crate::FinalityStatus;
use tokio::sync::watch::Receiver;

pub trait ReadFinality: Send + Sync + 'static {
    /// Get latest finality status.
    fn get_finality_status(&self) -> FinalityStatus;

    fn subscribe(&self) -> Receiver<FinalityStatus>;
}

pub trait WriteFinality: ReadFinality {
    /// Update latest finality status with provided function.
    fn update_finality_status(&self, f: impl FnOnce(&mut FinalityStatus));
}
