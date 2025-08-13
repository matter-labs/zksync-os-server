use crate::FinalityStatus;

pub trait ReadFinality: Send + Sync + 'static {
    /// Get latest finality status.
    fn get_finality_status(&self) -> FinalityStatus;
}

pub trait WriteFinality: ReadFinality {
    /// Update latest finality status with provided function.
    fn update_finality_status(&self, f: impl FnOnce(&mut FinalityStatus));
}
