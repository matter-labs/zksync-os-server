use crate::PipelineComponent;
use crate::peekable_receiver::PeekableReceiver;
use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::mpsc;

/// A named pipeline task: component name and its spawnable task function
type PipelineTask = (&'static str, BoxFuture<'static, Result<()>>);

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    tasks: Vec<PipelineTask>,
    receiver: PeekableReceiver<Output>,
}

impl Default for Pipeline<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl Pipeline<()> {
    pub fn new() -> Self {
        let (_sender, receiver) = mpsc::channel(1);
        Self {
            tasks: vec![],
            receiver: PeekableReceiver::new(receiver),
        }
    }

    /// Spawn all pipeline component tasks into a JoinSet
    pub fn spawn(self, tasks: &mut tokio::task::JoinSet<()>) {
        // Spawn all component tasks into JoinSet (these run indefinitely)
        for (name, task_fn) in self.tasks {
            tasks.spawn(async move {
                match task_fn.await {
                    Ok(_) => tracing::warn!("{name} component unexpectedly exited"),
                    Err(err) => tracing::error!(?err, "{name} component failed"),
                }
            });
        }
        // Drop the receiver - for terminal pipelines we don't need it
        drop(self.receiver);
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

        self.tasks.push((
            C::NAME,
            async move { component.run(input_receiver, output_sender).await }.boxed(),
        ));

        Pipeline {
            tasks: self.tasks,
            receiver: PeekableReceiver::new(output_receiver),
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

        self.tasks.push((
            C::NAME,
            async move { component.run(input_receiver, output_sender).await }.boxed(),
        ));

        Pipeline {
            tasks: self.tasks,
            receiver: PeekableReceiver::new(output_receiver),
        }
    }
}
