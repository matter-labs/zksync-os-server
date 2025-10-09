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

    /// Consume the pipeline and return the output receiver
    ///
    /// This spawns all tasks and returns the receiver for the final output.
    /// Useful when you need to manually consume the pipeline output with another component.
    pub fn into_receiver(self) -> PeekableReceiver<Output> {
        // Spawn all tasks
        for task_fn in self.tasks {
            task_fn();
        }

        self.receiver
    }
}
