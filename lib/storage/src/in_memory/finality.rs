use std::sync::{Arc, RwLock};
use zksync_os_storage_api::{FinalityStatus, ReadFinality, WriteFinality};

#[derive(Debug, Clone)]
pub struct Finality {
    data: Arc<RwLock<FinalityStatus>>,
}

impl Finality {
    pub fn new(initial_status: FinalityStatus) -> Self {
        Self {
            data: Arc::new(RwLock::new(initial_status)),
        }
    }
}

impl ReadFinality for Finality {
    fn get_finality_status(&self) -> FinalityStatus {
        self.data.read().unwrap().clone()
    }
}

impl WriteFinality for Finality {
    fn update_finality_status(&self, f: impl FnOnce(&mut FinalityStatus)) {
        let mut data = self.data.write().unwrap();
        f(&mut data);
    }
}
