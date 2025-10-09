use crate::PipelineComponent;
use crate::peekable_receiver::PeekableReceiver;
use anyhow::{Context, Result};
use tokio::sync::mpsc;

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    tasks: Vec<Box<dyn FnOnce() -> tokio::task::JoinHandle<Result<()>> + Send>>,
    receiver: PeekableReceiver<Output>,
}

impl Default for Pipeline<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl Pipeline<()> {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            tasks: vec![Box::new(move || {
                tokio::spawn(
                    async move { sender.send(()).await.context("failed to send source input") },
                )
            })],
            receiver: PeekableReceiver::new(receiver),
        }
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

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move { component.run(input_receiver, output_sender).await })
        }));

        Pipeline {
            tasks: self.tasks,
            receiver: PeekableReceiver::new(output_receiver),
        }
    }

    /// Add a transformer component to the pipeline with prepended messages
    ///
    /// This is useful when you need to reschedule messages at the start of the pipeline.
    /// The prepended messages are sent to the component before any messages from the pipeline.
    pub fn pipe_with_prepend<C>(mut self, component: C, prepend: Vec<Output>) -> Pipeline<C::Output>
    where
        C: PipelineComponent<Input = Output>,
    {
        let (output_sender, output_receiver) = mpsc::channel(C::OUTPUT_BUFFER_SIZE);
        let input_receiver = self.receiver.prepend(prepend);

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move { component.run(input_receiver, output_sender).await })
        }));

        Pipeline {
            tasks: self.tasks,
            receiver: PeekableReceiver::new(output_receiver),
        }
    }

    /// Spawn all pipeline component tasks into a JoinSet
    ///
    /// Use this for terminal pipelines ending with a Sink component.
    /// This spawns all component tasks into the provided JoinSet and drops the final receiver.
    ///
    /// The bootstrap task (from Pipeline::new) runs but is not tracked in the JoinSet since it
    /// completes after sending one message. Component tasks run indefinitely and are tracked.
    pub fn spawn(self, tasks: &mut tokio::task::JoinSet<()>) {
        let mut task_iter = self.tasks.into_iter();

        // Run the first task (bootstrap task from Pipeline::new) but don't track it
        // This task sends one () and exits - that's expected behavior
        if let Some(bootstrap_fn) = task_iter.next() {
            bootstrap_fn(); // Spawn into runtime but don't track
        }

        // Spawn remaining component tasks into JoinSet (these run indefinitely)
        for task_fn in task_iter {
            let handle = task_fn();
            tasks.spawn(async move {
                if let Err(e) = handle.await {
                    tracing::error!("Pipeline task panicked: {e:?}");
                }
            });
        }

        // Drop the receiver - for terminal pipelines we don't need it
        drop(self.receiver);
    }
}
