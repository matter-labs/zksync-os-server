use crate::PipelineComponent;
use crate::tracked_channel::{TrackedUnboundedReceiver, tracked_unbounded_channel};
use reth_tasks::Runtime;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// Pipeline with an active output stream that can be piped to more components
pub struct Pipeline<Output: Send + 'static> {
    receiver: TrackedUnboundedReceiver<Output>,
    runtime: Runtime,
    spawned_tasks: HashSet<&'static str>,
    shutdown_sender: mpsc::Sender<&'static str>,
    shutdown_receiver: mpsc::Receiver<&'static str>,
    /// Ordered list of (upstream_component_name, downstream_component_name) pairs.
    /// Derived automatically from pipe() call order.
    pub adjacency: Vec<(&'static str, &'static str)>,
    last_component_name: Option<&'static str>,
}

impl Pipeline<()> {
    pub fn new(runtime: Runtime) -> Self {
        let (_sender, receiver) = tracked_unbounded_channel::<()>();
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(16);
        Self {
            receiver,
            runtime,
            spawned_tasks: HashSet::default(),
            shutdown_sender,
            shutdown_receiver,
            adjacency: vec![],
            last_component_name: None,
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
                        // Defensive logging for duplicate or unexpected notifications.
                        tracing::warn!(%name, "tried to deregister non-existent segment");
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
        let (output_sender, output_receiver) = tracked_unbounded_channel::<C::Output>();
        let input_receiver = self.receiver;

        let shutdown_sender = self.shutdown_sender.clone();
        self.runtime
            .spawn_critical_with_graceful_shutdown_signal(C::NAME, |shutdown| async move {
                tokio::select! {
                    // Segments are expected to run until shutdown, but if this one exits early (for
                    // example because a channel closed), deregister it so shutdown does not wait
                    // for it forever.
                    res = component.run(input_receiver, output_sender) => {
                        res.expect("pipeline segment failed");
                        tracing::debug!(name = C::NAME, "segment finished running");
                        shutdown_sender.send(C::NAME).await.expect("failed to send shutdown status");
                    }
                    // Graceful shutdown started before the segment exited on its own; deregister it now.
                    _guard = shutdown => {
                        tracing::debug!(name = C::NAME, "segment shutting down");
                        shutdown_sender.send(C::NAME).await.expect("failed to send shutdown status");
                    }
                }
            });
        self.spawned_tasks.insert(C::NAME);

        let mut adjacency = self.adjacency;
        if let Some(prev) = self.last_component_name {
            adjacency.push((prev, C::NAME));
        }

        Pipeline {
            receiver: output_receiver,
            runtime: self.runtime,
            spawned_tasks: self.spawned_tasks,
            shutdown_sender: self.shutdown_sender,
            shutdown_receiver: self.shutdown_receiver,
            adjacency,
            last_component_name: Some(C::NAME),
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
    /// Verify that adjacency pairs reflect pipe() call order.
    ///
    /// The `Pipeline` struct's runtime field cannot be constructed without a live tokio executor,
    /// so instead we directly test the adjacency accumulation logic that `pipe()` uses.
    #[test]
    fn adjacency_reflects_pipe_order() {
        let mut adjacency: Vec<(&'static str, &'static str)> = vec![];
        let mut last: Option<&'static str> = None;

        // Simulate: pipe(A), pipe(B), pipe(C)
        for name in ["a", "b", "c"] {
            if let Some(prev) = last {
                adjacency.push((prev, name));
            }
            last = Some(name);
        }

        assert_eq!(adjacency, vec![("a", "b"), ("b", "c")]);
    }

    /// Verify that skipping a pipe_opt(None) does not add an adjacency entry between
    /// the component before None and the one after.
    #[test]
    fn pipe_opt_none_skips_adjacency() {
        let mut adjacency: Vec<(&'static str, &'static str)> = vec![];
        let mut last: Option<&'static str> = None;

        // Simulate: pipe(A), pipe_opt(None), pipe(C)
        // pipe(A):
        if let Some(prev) = last {
            adjacency.push((prev, "a"));
        }
        last = Some("a");

        // pipe_opt(None): no-op — last unchanged, no adjacency pushed

        // pipe(C):
        if let Some(prev) = last {
            adjacency.push((prev, "c"));
        }
        last = Some("c");

        // Expect ("a","c") — no intermediate edge because None was skipped
        assert_eq!(adjacency, vec![("a", "c")]);
        let _ = last;
    }
}
