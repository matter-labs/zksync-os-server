use crate::peekable_receiver::PeekableReceiver;
use crate::{PipelineComponent, Source};
use anyhow::Result;
use tokio::sync::mpsc;

/// Fluent pipeline builder with automatic channel wiring
///
/// Example:
/// ```ignore
/// Pipeline::new()
///     .pipe(CommandSource { ... })  // Source component
///     .pipe(Sequencer { ... })      // Transformer component
///     .pipe(TreeManager { ... })    // Transformer component
///     .execute()
///     .await
/// ```
pub struct Pipeline {
    tasks: Vec<Box<dyn FnOnce() -> tokio::task::JoinHandle<Result<()>> + Send>>,
}

impl Pipeline {
    /// Create a new empty pipeline
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Add a source component to start the pipeline
    pub fn pipe<S>(mut self, source: S) -> PipelineWithOutput<S::Output>
    where
        S: Source,
    {
        let (sender, receiver) = mpsc::channel(S::OUTPUT_BUFFER_SIZE);

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move { source.run(sender).await })
        }));

        PipelineWithOutput {
            tasks: self.tasks,
            receiver: PeekableReceiver::new(receiver),
        }
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Pipeline with an active output stream that can be piped to more components
pub struct PipelineWithOutput<Output: Send + 'static> {
    tasks: Vec<Box<dyn FnOnce() -> tokio::task::JoinHandle<Result<()>> + Send>>,
    receiver: PeekableReceiver<Output>,
}

impl<Output: Send + 'static> PipelineWithOutput<Output> {
    /// Add a transformer component to the pipeline
    pub fn pipe<C>(mut self, component: C) -> PipelineWithOutput<C::Output>
    where
        C: PipelineComponent<Input = Output>,
    {
        let (output_sender, output_receiver) = mpsc::channel(C::OUTPUT_BUFFER_SIZE);
        let input_receiver = self.receiver;

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move { component.run(input_receiver, output_sender).await })
        }));

        PipelineWithOutput {
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
