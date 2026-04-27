use crate::monitor::PipelineSnapshot;
use futures::stream::{StreamExt, select_all};
use reth_tasks::Runtime;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use zksync_os_pipeline::ComponentStateReceivers;

/// Aggregates all component state receivers into a single `watch::Receiver<PipelineSnapshot>`.
pub struct PipelineTracker;

impl PipelineTracker {
    pub fn spawn(
        runtime: &Runtime,
        components: ComponentStateReceivers,
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
        components: ComponentStateReceivers,
    ) {
        let streams = components
            .iter()
            .map(|(_, rx)| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ComponentId;
    use tokio::sync::watch;
    use zksync_os_observability::ComponentStateReporter;

    #[tokio::test]
    async fn tracker_republishes_on_state_change() {
        let (reporter, rx) = ComponentStateReporter::new("block_executor");
        reporter.record_processed(42, None, None);
        let components = vec![(ComponentId::BlockExecutor, rx)];

        let initial: PipelineSnapshot = components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().clone()))
            .collect();
        let (tx, mut snapshot_rx) = watch::channel(initial);
        tokio::spawn(PipelineTracker::run(tx, components));

        reporter.record_processed(100, None, None);
        snapshot_rx.changed().await.unwrap();

        assert_eq!(
            snapshot_rx
                .borrow()
                .iter()
                .find(|(id, _)| *id == ComponentId::BlockExecutor)
                .and_then(|(_, h)| h.block_processed.as_ref())
                .map(|c| c.block_number),
            Some(100)
        );
    }
}
