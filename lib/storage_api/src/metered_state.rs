use alloy::primitives::B256;
use zksync_os_interface::traits::{PreimageSource, ReadStorage};

#[derive(Debug, Clone)]
pub struct MeteredViewState<V> {
    pub state_view: V,
}

impl<V: ReadStorage> ReadStorage for MeteredViewState<V> {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.state_view.read(key)
    }
}

impl<V: PreimageSource> PreimageSource for MeteredViewState<V> {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.state_view.get_preimage(hash)
    }
}
