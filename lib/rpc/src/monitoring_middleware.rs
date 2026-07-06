use crate::metrics::{API_METRICS, RPC_TASK_MONITOR};
use crate::result::internal_rpc_err;
use futures::FutureExt as _;
use jsonrpsee::core::middleware::{Batch, BatchEntry, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::{BatchResponseBuilder, MethodResponse};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum CallKind {
    Call,
    Notification,
}

/// Bench knob: sample the per-call task-monitor instrumentation and size/latency histograms
/// 1-in-N (`RPC_CALL_METRICS_SAMPLE`, default 1 = every call, i.e. no behaviour change). Every
/// call otherwise does ~10 atomic RMWs on shared metric cachelines from every connection task;
/// at several 100k calls/s across many CCDs the cacheline ping-pong itself becomes the ingestion
/// ceiling. Error / cancelled / panicked counters stay exact regardless of sampling.
fn call_metrics_sample_stride() -> u64 {
    static STRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *STRIDE.get_or_init(|| {
        std::env::var("RPC_CALL_METRICS_SAMPLE")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|stride| *stride > 0)
            .unwrap_or(1)
    })
}

fn call_metrics_sampled() -> bool {
    let stride = call_metrics_sample_stride();
    if stride <= 1 {
        return true;
    }
    // Contention-free sampling: a shared counter would be its own convoy.
    thread_local! {
        static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    COUNTER.with(|counter| {
        let value = counter.get().wrapping_add(1);
        counter.set(value);
        value % stride == 0
    })
}

/// Bench-only (`RPC_ADMISSION_PROFILE`): sample 1-in-N batches (`RPC_BATCH_PROFILE_STRIDE`,
/// default 2048) for a phase-timing log line, splitting the server-side batch turnaround into
/// entry spawn vs join. Thread-local stride — same contention-free pattern as
/// `call_metrics_sampled`.
fn batch_profile_sampled() -> bool {
    static STRIDE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let Some(stride) = *STRIDE.get_or_init(|| {
        let enabled = std::env::var("RPC_ADMISSION_PROFILE")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        enabled.then(|| {
            std::env::var("RPC_BATCH_PROFILE_STRIDE")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|stride| *stride > 0)
                .unwrap_or(2048)
        })
    }) else {
        return false;
    };
    thread_local! {
        static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    COUNTER.with(|counter| {
        let value = counter.get().wrapping_add(1);
        counter.set(value);
        value % stride == 0
    })
}

#[derive(Clone)]
pub struct Monitoring<S = RpcService> {
    inner: S,
    max_response_size_bytes: usize,
}

impl<S> Monitoring<S> {
    pub fn new(inner: S, max_response_size_bytes: u32) -> Self {
        Self {
            inner,
            max_response_size_bytes: max_response_size_bytes as usize,
        }
    }
}

/// Ensures latency is recorded even if the future is dropped mid-flight (client disconnected).
struct CallGuard {
    kind: CallKind,
    method: String,
    started: Instant,
    request_size: usize,
    /// `Some((output_size, error_code))` once the future has resolved.
    completed: Option<(usize, Option<i32>)>,
    panicked: bool,
    /// Whether this call records the (shared-cacheline) size/latency histograms — see
    /// `call_metrics_sampled`.
    sampled: bool,
}

impl CallGuard {
    fn new(kind: CallKind, method: String, request_size: usize, sampled: bool) -> Self {
        Self {
            kind,
            method,
            started: Instant::now(),
            request_size,
            completed: None,
            panicked: false,
            sampled,
        }
    }

    async fn handle_result<F>(
        mut self,
        fut: F,
        on_panic: impl FnOnce() -> MethodResponse + Send,
    ) -> MethodResponse
    where
        F: Future<Output = MethodResponse> + Send,
    {
        let result = AssertUnwindSafe(fut).catch_unwind().await;
        self.panicked = result.is_err();
        let out = result.unwrap_or_else(|_| on_panic());
        self.completed = Some((out.as_json().get().len(), out.as_error_code()));
        out
    }
}

/// Ensures batch-level metrics are recorded even if the future is dropped mid-flight (client disconnected).
struct BatchGuard {
    batch_input_size: usize,
    request_counts: HashMap<String, u64>,
    started: Instant,
    /// `Some(response_size)` once the batch has resolved.
    completed: Option<usize>,
}

impl BatchGuard {
    fn new(batch_input_size: usize, request_counts: HashMap<String, u64>) -> Self {
        Self {
            batch_input_size,
            request_counts,
            started: Instant::now(),
            completed: None,
        }
    }
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let cancelled = self.completed.is_none();
        let response_size = self.completed.take().unwrap_or(0);
        if cancelled {
            API_METRICS.cancelled["batch"].inc();
        }
        API_METRICS.response_time["batch"].observe(elapsed);
        API_METRICS.request_size["batch"].observe(self.batch_input_size);
        API_METRICS.response_size["batch"].observe(response_size);
        for (method, count) in &self.request_counts {
            API_METRICS.requests_in_batch_count[method.as_str()].observe(*count);
        }
        tracing::debug!(
            target: "rpc::monitoring::batch",
            cancelled,
            "rpc batch call completed cancelled={}", cancelled
        );
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let cancelled = self.completed.is_none();
        let (output_size, error_code) = self.completed.take().unwrap_or((0, None));
        if self.sampled {
            API_METRICS.response_time[&self.method].observe(elapsed);
            API_METRICS.request_size[&self.method].observe(self.request_size);
            API_METRICS.response_size[&self.method].observe(output_size);
        }
        if let Some(code) = error_code {
            API_METRICS.errors[&(self.method.clone(), code)].inc();
        }
        if cancelled {
            API_METRICS.cancelled[&self.method].inc();
        }
        if self.panicked {
            API_METRICS.panicked[&self.method].inc();
            match self.kind {
                CallKind::Call => tracing::error!(method = %self.method, "RPC handler panicked"),
                CallKind::Notification => {
                    tracing::error!(method = %self.method, "Notification handler panicked")
                }
            }
        }

        macro_rules! log {
            ($target:literal) => {
                tracing::debug!(
                    target: $target,
                    kind = ?self.kind,
                    cancelled,
                    "rpc call completed kind={:?} cancelled={}", self.kind, cancelled
                )
            };
        }

        match self.method.as_str() {
            "eth_call" => log!("rpc::monitoring::eth::call"),
            "eth_sendRawTransaction" => log!("rpc::monitoring::eth::sendRawTransaction"),
            "debug_traceTransaction" => log!("rpc::monitoring::debug::traceTransaction"),
            _ => log!("rpc::monitoring::call"),
        }
    }
}

impl<S> RpcServiceT for Monitoring<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            NotificationResponse = MethodResponse,
            BatchResponse = MethodResponse,
        > + Clone
        + Send
        + 'static,
{
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;
    type BatchResponse = MethodResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let method = request.method_name().to_owned();
        let request_size = request.params.as_ref().map_or(0, |p| p.get().len());
        let inner = self.inner.clone();
        let sampled = call_metrics_sampled();

        async move {
            let id = request.id.clone().into_owned();
            let inner_call = async move { inner.call(request).await };
            // The task monitor's shared poll counters are part of the per-call metric convoy;
            // only instrument sampled calls.
            let handler = if sampled {
                futures::future::Either::Left(RPC_TASK_MONITOR.instrument(inner_call))
            } else {
                futures::future::Either::Right(inner_call)
            };
            let on_panic = || MethodResponse::error(id, internal_rpc_err("Internal error"));
            CallGuard::new(CallKind::Call, method, request_size, sampled)
                .handle_result(handler, on_panic)
                .await
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        // Collect some metrics about the batch
        let batch_input_size: usize = batch
            .iter()
            .filter_map(|x| {
                if let Ok(req) = x {
                    Some(req.params().as_ref().map_or(0, |p| p.get().len()))
                } else {
                    None
                }
            })
            .sum();

        let request_counts = batch
            .iter()
            .filter_map(|x| {
                if let Ok(req) = x {
                    Some(req.method_name().to_owned())
                } else {
                    None
                }
            })
            .fold(HashMap::new(), |mut acc, method| {
                *acc.entry(method).or_insert(0u64) += 1;
                acc
            });

        let mut batch_rp = BatchResponseBuilder::new_with_limit(self.max_response_size_bytes);
        let service = self.clone();
        async move {
            let mut guard = BatchGuard::new(batch_input_size, request_counts);
            let mut got_notification = false;
            let profile_started = batch_profile_sampled().then(Instant::now);

            // Run the batch's calls in PARALLEL: each is spawned onto the runtime so CPU-heavy
            // handlers (e.g. `eth_sendRawTransaction`'s ECDSA recovery) spread across worker
            // threads instead of serializing on this connection's task — a sequential loop here
            // caps ingestion at `connections × 1/per-call-cost` no matter how many cores exist.
            // Response ORDER is preserved: handles are awaited in request order. (jsonrpsee's
            // own default batch handling is concurrent too; spawning additionally buys
            // parallelism.)
            enum PendingEntry {
                Spawned(
                    jsonrpsee::types::Id<'static>,
                    tokio::task::JoinHandle<MethodResponse>,
                ),
                Ready(MethodResponse),
            }
            let mut entries = Vec::new();
            for batch_entry in batch.into_iter() {
                match batch_entry {
                    Ok(BatchEntry::Call(mut req)) => {
                        let id = req.id.clone().into_owned();
                        // Spawning needs a `'static` request; rebuild it from owned parts,
                        // carrying the extensions over (they hold per-connection context).
                        let mut owned = Request::owned(
                            req.method_name().to_owned(),
                            req.params.take().map(|params| params.into_owned()),
                            id.clone(),
                        );
                        *owned.extensions_mut() = std::mem::take(req.extensions_mut());
                        let service = service.clone();
                        entries.push(PendingEntry::Spawned(
                            id,
                            tokio::spawn(async move { service.call(owned).await }),
                        ));
                    }
                    Ok(BatchEntry::Notification(n)) => {
                        got_notification = true;
                        service.notification(n).await;
                    }
                    Err(err) => {
                        let (err, id) = err.into_parts();
                        entries.push(PendingEntry::Ready(MethodResponse::error(id, err)));
                    }
                }
            }
            let spawned_at = profile_started.map(|_| Instant::now());
            let entry_count = entries.len();
            for entry in entries {
                let rp = match entry {
                    PendingEntry::Spawned(id, handle) => match handle.await {
                        Ok(rp) => rp,
                        // Panics inside the call are already caught by `CallGuard`; a join
                        // error only means the task was aborted (runtime shutdown).
                        Err(_) => MethodResponse::error(id, internal_rpc_err("Internal error")),
                    },
                    PendingEntry::Ready(rp) => rp,
                };
                if let Err(err) = batch_rp.append(rp) {
                    return err;
                }
            }

            // If the batch is empty, and we got a notification, we return an empty response.
            let response = if batch_rp.is_empty() && got_notification {
                MethodResponse::notification()
            } else {
                MethodResponse::from_batch(batch_rp.finish())
            };

            if let (Some(t0), Some(t1)) = (profile_started, spawned_at) {
                tracing::error!(
                    entries = entry_count,
                    spawn = ?t1.duration_since(t0),
                    join = ?t1.elapsed(),
                    total = ?t0.elapsed(),
                    "rpc batch profile"
                );
            }

            guard.completed = Some(response.as_json().get().len());
            response
        }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let request_size = n.params.as_ref().map_or(0, |p| p.get().len());
        let method = n.method_name().to_owned();
        let inner = self.inner.clone();

        let sampled = call_metrics_sampled();
        async move {
            let handler = async move { inner.notification(n).await };
            CallGuard::new(CallKind::Notification, method, request_size, sampled)
                .handle_result(handler, MethodResponse::notification)
                .await
        }
    }
}
