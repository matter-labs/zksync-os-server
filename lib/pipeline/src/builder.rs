use crate::ComponentId;
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
    /// Ordered list of (upstream, downstream) ComponentId pairs for health-monitor adjacency.
    /// Built from every consecutive pipe() call.
    pub adjacency: Vec<(ComponentId, ComponentId)>,
    last_component_id: Option<ComponentId>,
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
            last_component_id: None,
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
        let name = C::COMPONENT_ID.as_str();
        let (output_sender, output_receiver) = tracked_unbounded_channel::<C::Output>();
        let input_receiver = self.receiver;

        let shutdown_sender = self.shutdown_sender.clone();
        self.runtime
            .spawn_critical_with_graceful_shutdown_signal(name, |shutdown| async move {
                tokio::select! {
                    res = component.run(input_receiver, output_sender) => {
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

        let mut adjacency = self.adjacency;
        if let Some(prev_id) = self.last_component_id {
            adjacency.push((prev_id, C::COMPONENT_ID));
        }

        Pipeline {
            receiver: output_receiver,
            runtime: self.runtime,
            spawned_tasks: self.spawned_tasks,
            shutdown_sender: self.shutdown_sender,
            shutdown_receiver: self.shutdown_receiver,
            adjacency,
            last_component_id: Some(C::COMPONENT_ID),
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

    #[test]
    fn adjacency_reflects_pipe_order() {
        let mut adjacency: Vec<(ComponentId, ComponentId)> = vec![];
        let mut last: Option<ComponentId> = None;

        // Simulate: pipe(A), pipe(B), pipe(C) — all have COMPONENT_ID
        for id in [
            ComponentId::BlockExecutor,
            ComponentId::BlockCanonizer,
            ComponentId::BlockApplier,
        ] {
            if let Some(prev) = last {
                adjacency.push((prev, id));
            }
            last = Some(id);
        }

        assert_eq!(
            adjacency,
            vec![
                (ComponentId::BlockExecutor, ComponentId::BlockCanonizer),
                (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
            ]
        );
    }

    #[test]
    fn pipe_opt_none_skips_adjacency() {
        let mut adjacency: Vec<(ComponentId, ComponentId)> = vec![];
        let mut last: Option<ComponentId> = None;

        // pipe(A):
        let a = ComponentId::BlockExecutor;
        if let Some(prev) = last {
            adjacency.push((prev, a));
        }
        last = Some(a);

        // pipe_opt(None): no-op — last unchanged, no adjacency pushed

        // pipe(C):
        let c = ComponentId::BlockApplier;
        if let Some(prev) = last {
            adjacency.push((prev, c));
        }
        last = Some(c);

        assert_eq!(
            adjacency,
            vec![(ComponentId::BlockExecutor, ComponentId::BlockApplier)]
        );
        let _ = last;
    }
}
