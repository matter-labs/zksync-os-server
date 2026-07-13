use crate::PipelineComponent;
use crate::component_id::ComponentId;
use crate::peekable_receiver::PeekableReceiver;
use reth_tasks::Runtime;
use std::collections::HashSet;
use tokio::sync::{mpsc, watch};
use zksync_os_observability::{ComponentState, ComponentStateReporter};

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    receiver: PeekableReceiver<Output>,
    runtime: Runtime,
    spawned_tasks: HashSet<&'static str>,
    shutdown_sender: mpsc::Sender<&'static str>,
    shutdown_receiver: mpsc::Receiver<&'static str>,
    components: Vec<(ComponentId, watch::Receiver<ComponentState>)>,
}

impl<Output: Send + 'static> Pipeline<Output> {
    pub fn components(&self) -> Vec<(ComponentId, watch::Receiver<ComponentState>)> {
        self.components
            .iter()
            .map(|(id, rx)| (*id, rx.clone()))
            .collect()
    }
}

impl Pipeline<()> {
    pub fn new(runtime: Runtime) -> Self {
        let (_sender, receiver) = mpsc::channel::<()>(1);
        let receiver = PeekableReceiver::new(receiver);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(16);
        Self {
            receiver,
            runtime,
            spawned_tasks: HashSet::default(),
            shutdown_sender,
            shutdown_receiver,
            components: Vec::new(),
        }
    }

    /// Spawns a final supervisor that waits for all pipeline segments to shut down.
    /// Returns the accumulated component state receivers for backpressure monitoring.
    pub fn spawn(mut self) {
        // No consumer exists after the terminal stage.
        drop(self.receiver);

        self.runtime.spawn_critical_with_graceful_shutdown_signal(
            "pipeline",
            |shutdown| async move {
                // Hold shutdown open until every spawned segment deregisters.
                let _guard = shutdown.await;

                while !self.spawned_tasks.is_empty() {
                    // Each segment sends its name when it exits or handles shutdown.
                    let Some(name) = self.shutdown_receiver.recv().await else {
                        // A segment that panicked (or was torn down hard) drops its
                        // sender without deregistering. The supervisor's job is
                        // bookkeeping — it must not amplify that into a second
                        // panic.
                        tracing::warn!(
                            remaining = ?self.spawned_tasks,
                            "pipeline segment(s) never deregistered"
                        );
                        break;
                    };

                    if !self.spawned_tasks.remove(name) {
                        // Defensive logging for duplicate or unexpected notifications.
                        tracing::warn!(%name, "tried to deregister non-existent segment");
                    } else {
                        tracing::debug!(%name, "pipeline segment deregistered");
                    }

                    if !self.spawned_tasks.is_empty() {
                        tracing::debug!("pipeline segments left: {:?}", self.spawned_tasks);
                    }
                }

                if self.spawned_tasks.is_empty() {
                    tracing::debug!("pipeline finished gracefully");
                }
            },
        );
    }
}

impl<Output: Send + 'static> Pipeline<Output> {
    /// Add a transformer component to the pipeline
    pub fn pipe<C>(mut self, component: C) -> Pipeline<C::Output>
    where
        C: PipelineComponent<Input = Output>,
    {
        let id = C::COMPONENT_ID;
        let name = id.as_str();

        let (reporter, rx) = ComponentStateReporter::new(name);
        self.components.push((id, rx));

        let (output_sender, output_receiver) =
            mpsc::channel::<C::Output>(C::OUTPUT_CHANNEL_CAPACITY);
        let output_receiver = PeekableReceiver::new(output_receiver);
        let input_receiver = self.receiver;

        let shutdown_sender = self.shutdown_sender.clone();
        self.runtime
            .spawn_critical_with_graceful_shutdown_signal(name, |shutdown| async move {
                // `biased` + shutdown polled first: once the shutdown signal is set,
                // segments exit in arbitrary order, and an upstream exiting first
                // closes this segment's input — making `run` return an error that
                // is *normal wind-down*, not a failure. A segment can also fail
                // *because of* shutdown without its input closing — a downstream
                // receiver dropped first, or the segment's own I/O severed as the
                // node winds down — so an error re-checks the signal below before
                // being declared fatal. A segment error while the node is actually
                // running still panics loudly.
                let mut shutdown = shutdown;
                tokio::select! {
                    biased;
                    _guard = &mut shutdown => {
                        tracing::debug!(name, "segment shutting down");
                    }
                    res = component.run(input_receiver, output_sender, reporter) => {
                        match res {
                            Ok(()) => tracing::debug!(name, "segment finished running"),
                            Err(err) => match futures::FutureExt::now_or_never(&mut shutdown) {
                                Some(_guard) => {
                                    tracing::debug!(name, %err, "segment errored during shutdown");
                                }
                                None => panic!("pipeline segment failed: {err:?}"),
                            },
                        }
                    }
                }
                // Always deregister, even from a failing teardown; the supervisor
                // itself may already be gone, so a failed send is not an error.
                shutdown_sender.send(name).await.ok();
            });
        self.spawned_tasks.insert(name);

        Pipeline {
            receiver: output_receiver,
            runtime: self.runtime,
            spawned_tasks: self.spawned_tasks,
            shutdown_sender: self.shutdown_sender,
            shutdown_receiver: self.shutdown_receiver,
            components: self.components,
        }
    }

    /// Conditionally add a component if present. The component must keep the same item type.
    pub fn pipe_opt<C>(self, component: Option<C>) -> Pipeline<Output>
    where
        C: PipelineComponent<Input = Output, Output = Output>,
    {
        match component {
            Some(c) => self.pipe(c),
            None => self,
        }
    }

    /// Conditional add one component or the other. Both components need to have same item types.
    pub fn pipe_if<CTrue, CFalse>(
        self,
        condition: bool,
        c_true: CTrue,
        c_false: CFalse,
    ) -> Pipeline<CTrue::Output>
    where
        CTrue: PipelineComponent<Input = Output>,
        CFalse: PipelineComponent<Input = Output, Output = CTrue::Output>,
    {
        match condition {
            true => self.pipe(c_true),
            false => self.pipe(c_false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use zksync_os_observability::ComponentStateReporter;

    /// A segment that reports in, waits for `go`, then fails after a
    /// *synchronous* pause. Synchronous on purpose: the task must not yield
    /// between `go` and the error, or the `biased` shutdown arm would win the
    /// next poll and absorb the error before it ever happens — the pause is
    /// the window in which a test fires (or doesn't fire) the shutdown signal,
    /// which is exactly the teardown race the discrimination in `pipe()`
    /// exists for.
    struct FailingSegment {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        go: tokio::sync::oneshot::Receiver<()>,
    }

    #[async_trait]
    impl PipelineComponent for FailingSegment {
        type Input = ();
        type Output = ();
        const COMPONENT_ID: ComponentId = ComponentId::NoopSink;

        async fn run(
            mut self,
            _input: PeekableReceiver<()>,
            _output: mpsc::Sender<()>,
            _reporter: ComponentStateReporter,
        ) -> anyhow::Result<()> {
            self.started
                .take()
                .expect("run is called once")
                .send(())
                .ok();
            self.go.await.ok();
            std::thread::sleep(Duration::from_millis(300));
            anyhow::bail!("downstream closed")
        }
    }

    /// Counts every panic in the process (the hook still chains to the default
    /// one). Reliable as a test assertion because nextest runs each test in
    /// its own process.
    fn install_panic_counter() -> Arc<AtomicUsize> {
        let counter = Arc::new(AtomicUsize::new(0));
        let hook_counter = counter.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hook_counter.fetch_add(1, Ordering::SeqCst);
            previous(info);
        }));
        counter
    }

    fn test_runtime() -> Runtime {
        RuntimeBuilder::new(
            RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(
                tokio::runtime::Handle::current(),
            )),
        )
        .build()
        .expect("failed to build runtime")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_segment_error_during_shutdown_winds_down_without_panicking() {
        let panics = install_panic_counter();
        let runtime = test_runtime();
        let (started_sender, started) = tokio::sync::oneshot::channel();
        let (go, go_receiver) = tokio::sync::oneshot::channel();
        Pipeline::new(runtime.clone())
            .pipe(FailingSegment {
                started: Some(started_sender),
                go: go_receiver,
            })
            .spawn();

        started.await.expect("segment started");
        go.send(()).expect("segment waits for go");
        // The segment is now inside its synchronous pause; fire the shutdown
        // signal so its failure lands *after* shutdown began — the batcher's
        // sidecar send and the l1 senders hit this exact interleaving on a
        // real SIGTERM.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(
            runtime
                .initiate_graceful_shutdown()
                .expect("task manager is alive"),
        );

        assert!(
            runtime.graceful_shutdown_with_timeout(Duration::from_secs(10)),
            "every segment must deregister and wind down"
        );
        assert_eq!(
            panics.load(Ordering::SeqCst),
            0,
            "an error during shutdown is wind-down, not a failure"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_segment_error_while_live_still_panics_exactly_once() {
        let panics = install_panic_counter();
        let runtime = test_runtime();
        let (started_sender, started) = tokio::sync::oneshot::channel();
        let (go, go_receiver) = tokio::sync::oneshot::channel();
        Pipeline::new(runtime.clone())
            .pipe(FailingSegment {
                started: Some(started_sender),
                go: go_receiver,
            })
            .spawn();

        started.await.expect("segment started");
        // No shutdown signal: the failure lands while the node is live.
        go.send(()).expect("segment waits for go");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while panics.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a live segment failure must panic"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Tear down; the panicked segment never deregistered, and the
        // supervisor must absorb that instead of cascading into a second
        // panic.
        runtime.graceful_shutdown_with_timeout(Duration::from_secs(10));
        assert_eq!(
            panics.load(Ordering::SeqCst),
            1,
            "exactly the segment's own panic — the supervisor must not amplify it"
        );
    }
}
