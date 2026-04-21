use crate::PipelineComponent;
use crate::component_id::ComponentId;
use crate::monitor::PipelineMonitor;
use crate::peekable_receiver::PeekableReceiver;
use reth_tasks::Runtime;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateReporter;

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    receiver: PeekableReceiver<Output>,
    runtime: Runtime,
    spawned_tasks: HashSet<&'static str>,
    shutdown_sender: mpsc::Sender<&'static str>,
    shutdown_receiver: mpsc::Receiver<&'static str>,
    monitor: Arc<dyn PipelineMonitor>,
    prev_id: Option<ComponentId>,
}

impl Pipeline<()> {
    pub fn new(runtime: Runtime, monitor: Arc<dyn PipelineMonitor>) -> Self {
        let (_sender, receiver) = mpsc::unbounded_channel::<()>();
        let receiver = PeekableReceiver::new(receiver);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(16);
        Self {
            receiver,
            runtime,
            spawned_tasks: HashSet::default(),
            shutdown_sender,
            shutdown_receiver,
            monitor,
            prev_id: None,
        }
    }

    /// Spawns a final supervisor that waits for all pipeline segments to shut down.
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
                        panic!(
                            "failed to receive deregistration for segments: {:?}",
                            self.spawned_tasks
                        );
                    };

                    if !self.spawned_tasks.remove(name) {
                        panic!(
                            "deregistration for unknown pipeline segment {name:?}; \
                             known segments: {:?}",
                            self.spawned_tasks
                        );
                    }
                    tracing::debug!(%name, "pipeline segment deregistered");

                    if !self.spawned_tasks.is_empty() {
                        tracing::debug!("pipeline segments left: {:?}", self.spawned_tasks);
                    }
                }

                tracing::debug!("pipeline finished gracefully");
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

        let reporter = if C::REGISTER_WITH_MONITOR {
            let r = self.monitor.register(id, self.prev_id);
            self.prev_id = Some(id);
            r
        } else {
            ComponentStateReporter::new(name).0
        };

        let (output_sender, output_receiver) = mpsc::unbounded_channel::<C::Output>();
        let output_receiver = PeekableReceiver::new(output_receiver);
        let input_receiver = self.receiver;

        let shutdown_sender = self.shutdown_sender.clone();
        self.runtime
            .spawn_critical_with_graceful_shutdown_signal(name, |shutdown| async move {
                tokio::select! {
                    res = component.run(input_receiver, output_sender, reporter) => {
                        res.expect("pipeline segment failed");
                        tracing::debug!(name, "segment finished running");
                        shutdown_sender.send(name).await.expect("failed to send shutdown status");
                    }
                    _guard = shutdown => {
                        tracing::debug!(name, "segment shutting down");
                        shutdown_sender.send(name).await.expect("failed to send shutdown status");
                    }
                }
            });
        self.spawned_tasks.insert(name);

        Pipeline {
            receiver: output_receiver,
            runtime: self.runtime,
            spawned_tasks: self.spawned_tasks,
            shutdown_sender: self.shutdown_sender,
            shutdown_receiver: self.shutdown_receiver,
            monitor: self.monitor,
            prev_id: self.prev_id,
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
