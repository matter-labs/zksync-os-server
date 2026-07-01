use crate::monitor::PipelineSnapshot;
use futures::stream::{StreamExt, select_all};
use reth_tasks::Runtime;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use zksync_os_observability::ComponentState;
use zksync_os_pipeline::ComponentId;

/// Aggregates all component state receivers into a single `watch::Receiver<PipelineSnapshot>`.
pub struct PipelineTracker;

impl PipelineTracker {
    pub fn spawn(
        runtime: &Runtime,
        components: Vec<(ComponentId, watch::Receiver<ComponentState>)>,
    ) -> watch::Receiver<PipelineSnapshot> {
        let initial: PipelineSnapshot = components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().clone()))
            .collect();
        let (tx, rx) = watch::channel(initial);
        runtime.spawn_critical_task("pipeline tracker", Self::run(tx, components));
        rx
    }

    pub(crate) async fn run(
        tx: watch::Sender<PipelineSnapshot>,
        components: Vec<(ComponentId, watch::Receiver<ComponentState>)>,
    ) {
        let streams = components
            .iter()
            .map(|(_, rx)| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();

        let snapshot: PipelineSnapshot = components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().clone()))
            .collect();
        let _ = tx.send(snapshot);

        let mut combined = select_all(streams);
        while combined.next().await.is_some() {
            let snapshot: PipelineSnapshot = components
                .iter()
                .map(|(id, rx)| (*id, rx.borrow().clone()))
                .collect();
            if tx.send(snapshot).is_err() {
                break;
            }
        }
    }
}
