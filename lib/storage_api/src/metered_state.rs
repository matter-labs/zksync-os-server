use alloy::primitives::B256;
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_observability::{ComponentStateHandle, StateLabel};

pub trait StateAccessLabel: StateLabel {
    fn read_storage_state() -> Self;
    fn read_preimage_state() -> Self;
    fn default_execution_state() -> Self;
}

#[derive(Debug, Clone)]
pub struct MeteredViewState<T, V> {
    pub component_state_tracker: ComponentStateHandle<T>,
    pub state_view: V,
}

impl<T: StateAccessLabel, V> MeteredViewState<T, V> {
    #[inline]
    fn with_state<R>(&mut self, label: T, f: impl FnOnce(&mut V) -> R) -> R {
        self.component_state_tracker.enter_state(label);
        let res = f(&mut self.state_view);
        self.component_state_tracker
            .enter_state(T::default_execution_state());
        res
    }
}

impl<T: StateAccessLabel, V: ReadStorage> ReadStorage for MeteredViewState<T, V> {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.with_state(T::read_storage_state(), |view| view.read(key))
    }
}

impl<T: StateAccessLabel, V: PreimageSource> PreimageSource for MeteredViewState<T, V> {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.with_state(T::read_preimage_state(), |view| view.get_preimage(hash))
    }
}
