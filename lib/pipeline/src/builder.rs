use crate::PipelineComponent;
use crate::peekable_receiver::PeekableReceiver;
use reth_tasks::Runtime;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    receiver: PeekableReceiver<Output>,
    runtime: Runtime,

    spawned_tasks: HashSet<&'static str>,
    shutdown_sender: mpsc::Sender<&'static str>,
    shutdown_receiver: mpsc::Receiver<&'static str>,
}

impl Pipeline<()> {
    pub fn new(runtime: Runtime) -> Self {
        let (_sender, receiver) = mpsc::channel(1);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(16);
        Self {
            receiver: PeekableReceiver::new(receiver),
            runtime,
            spawned_tasks: HashSet::default(),
            shutdown_sender,
            shutdown_receiver,
        }
    }

    /// Spawn terminal pipeline segment that ensures graceful shutdown for all other segments.
    pub fn spawn(mut self) {
        // Drop the receiver - for terminal pipelines we don't need it
        drop(self.receiver);
        self.runtime.spawn_critical_with_graceful_shutdown_signal(
            "pipeline",
            |shutdown| async move {
                let _guard = shutdown.await;
                while !self.spawned_tasks.is_empty() {
                    let Some(name) = self.shutdown_receiver.recv().await else {
                        panic!(
                            "failed to receive deregistration for segments: {:?}",
                            self.spawned_tasks
                        );
                    };
                    if !self.spawned_tasks.remove(name) {
                        tracing::warn!("tried to deregister non-existent segment");
                    } else {
                        tracing::debug!(%name, "pipeline segment deregistered");
                    }
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
        let (output_sender, output_receiver) = mpsc::channel(C::OUTPUT_BUFFER_SIZE);
        let input_receiver = self.receiver;

        let shutdown_sender = self.shutdown_sender.clone();
        self.runtime
            .spawn_critical_with_graceful_shutdown_signal(C::NAME, |shutdown| async move {
                tokio::select! {
                    res = component.run(input_receiver, output_sender) => {
                        res.expect("pipeline segment failed");
                        tracing::debug!(name = C::NAME, "segment finished running");
                        shutdown_sender.send(C::NAME).await.expect("failed to send shutdown status");
                    }
                    _guard = shutdown => {
                        tracing::debug!(name = C::NAME, "segment shutting down");
                        shutdown_sender.send(C::NAME).await.expect("failed to send shutdown status");
                    }
                }
            });
        self.spawned_tasks.insert(C::NAME);

        Pipeline {
            receiver: PeekableReceiver::new(output_receiver),
            runtime: self.runtime,
            spawned_tasks: self.spawned_tasks,
            shutdown_sender: self.shutdown_sender,
            shutdown_receiver: self.shutdown_receiver,
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
