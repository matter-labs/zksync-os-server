use crate::component_id::ComponentId;
use zksync_os_observability::ComponentStateReporter;

/// Builder-facing view of a backpressure monitor.
///
/// `Pipeline::pipe()` uses this to create a reporter for each monitored component and
/// declare an upstream→downstream edge in one call. Non-pipe components call `register`
/// directly, naming their upstream explicitly at the call site.
pub trait PipelineMonitor: Send + Sync + 'static {
    fn register(&self, id: ComponentId, upstream: Option<ComponentId>) -> ComponentStateReporter;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(ComponentId, Option<ComponentId>)>>,
    }

    impl PipelineMonitor for Recorder {
        fn register(
            &self,
            id: ComponentId,
            upstream: Option<ComponentId>,
        ) -> ComponentStateReporter {
            self.calls.lock().unwrap().push((id, upstream));
            ComponentStateReporter::new(id.as_str()).0
        }
    }

    #[test]
    fn trait_is_object_safe_and_records_calls() {
        let rec = Arc::new(Recorder::default());
        let dyn_rec: Arc<dyn PipelineMonitor> = rec.clone();

        let _ = dyn_rec.register(ComponentId::BlockExecutor, None);
        let _ = dyn_rec.register(
            ComponentId::BlockCanonizer,
            Some(ComponentId::BlockExecutor),
        );
        let _ = dyn_rec.register(ComponentId::PriorityTree, Some(ComponentId::BlockApplier));

        let calls = rec.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], (ComponentId::BlockExecutor, None));
        assert_eq!(
            calls[1],
            (
                ComponentId::BlockCanonizer,
                Some(ComponentId::BlockExecutor),
            )
        );
        assert_eq!(
            calls[2],
            (ComponentId::PriorityTree, Some(ComponentId::BlockApplier),)
        );
    }
}
