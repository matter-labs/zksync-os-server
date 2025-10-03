use crate::peekable_receiver::PeekableReceiver;
use crate::{PipelineComponent, Sink, Source};
use anyhow::Result;
use std::mem::size_of;
use tokio::sync::mpsc;

/// A configured pipeline ready for execution
pub struct ConfiguredPipeline {
    tasks: Vec<Box<dyn FnOnce() -> tokio::task::JoinHandle<Result<()>> + Send>>,
}

impl ConfiguredPipeline {
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Add a source component to the pipeline
    pub fn add_source<S: Source + 'static>(
        &mut self,
        params: S::Params,
    ) -> PeekableReceiver<S::Output> {
        let (sender, receiver) = mpsc::channel(S::OUTPUT_BUFFER_SIZE);

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move {
                S::run(params, sender).await
            })
        }));

        PeekableReceiver::new(receiver)
    }

    /// Add a pipeline component that connects input receiver to output sender
    pub fn add_component<C: PipelineComponent + 'static>(
        &mut self,
        params: C::Params,
        input_receiver: PeekableReceiver<C::Input>,
    ) -> PeekableReceiver<C::Output> {

        // This trait only supports zero-sized structs.
        // Do not remove without a good reason.
        assert_eq!(
            size_of::<C>(),
            0,
            "PipelineComponent {} must be zero-sized (no fields). Use Params instead.",
            C::NAME
        );


        let (output_sender, output_receiver) = mpsc::channel(C::OUTPUT_BUFFER_SIZE);

        let input = input_receiver;

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move {
                C::run(params, input, output_sender).await
            })
        }));

        PeekableReceiver::new(output_receiver)
    }

    /// Add a sink component that consumes messages
    pub fn add_sink<S: Sink + 'static>(
        &mut self,
        params: S::Params,
        input_receiver: mpsc::Receiver<S::Input>,
    ) {
        let input = PeekableReceiver::new(input_receiver);

        self.tasks.push(Box::new(move || {
            tokio::spawn(async move {
                S::run(params, input).await
            })
        }));
    }

    /// Execute all components in the pipeline
    pub async fn execute(self) -> Result<()> {
        let mut handles = Vec::new();

        // Spawn all tasks
        for task_fn in self.tasks {
            let handle = task_fn();
            handles.push(handle);
        }

        // Wait for any task to complete (usually indicates an error)
        let (result, _index, _remaining) = futures::future::select_all(handles).await;

        match result {
            Ok(task_result) => task_result,
            Err(join_error) => anyhow::bail!("Task panicked: {}", join_error),
        }
    }
}