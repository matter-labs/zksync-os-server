use alloy::primitives::B256;
use std::marker::PhantomData;
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_observability::{ComponentStateReporter, StateLabel};

/// Extension of `StateLabel` that provides the three states used by `MeteredViewState`.
pub trait StateAccessLabel: StateLabel {
    fn read_storage_state() -> Self;
    fn read_preimage_state() -> Self;
    fn default_execution_state() -> Self;
}

/// Wraps a storage view and reports fine-grained state to a `ComponentStateReporter`
/// on every read, returning to the execution state afterward.
pub struct MeteredViewState<T, V> {
    pub component_state_reporter: ComponentStateReporter,
    pub state_view: V,
    _phantom: PhantomData<T>,
}

impl<T, V: Clone> Clone for MeteredViewState<T, V> {
    fn clone(&self) -> Self {
        Self {
            component_state_reporter: self.component_state_reporter.clone(),
            state_view: self.state_view.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<T: StateAccessLabel, V> MeteredViewState<T, V> {
    pub fn new(reporter: ComponentStateReporter, state_view: V) -> Self {
        Self {
            component_state_reporter: reporter,
            state_view,
            _phantom: PhantomData,
        }
    }

    #[inline]
    fn with_state<R>(&mut self, label: T, f: impl FnOnce(&mut V) -> R) -> R {
        self.component_state_reporter.enter_state(label);
        let res = f(&mut self.state_view);
        self.component_state_reporter
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
