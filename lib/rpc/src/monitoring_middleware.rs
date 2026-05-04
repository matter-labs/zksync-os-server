use crate::metrics::API_METRICS;
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

#[derive(Clone)]
pub struct Monitoring {
    inner: RpcService,
    max_response_size_bytes: usize,
}

impl Monitoring {
    pub fn new(inner: RpcService, max_response_size_bytes: u32) -> Self {
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
}

impl CallGuard {
    fn new(kind: CallKind, method: String, request_size: usize) -> Self {
        Self {
            kind,
            method,
            started: Instant::now(),
            request_size,
            completed: None,
            panicked: false,
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
        API_METRICS.response_time[&self.method].observe(elapsed);
        API_METRICS.request_size[&self.method].observe(self.request_size);
        API_METRICS.response_size[&self.method].observe(output_size);
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

impl RpcServiceT for Monitoring {
    type MethodResponse = <RpcService as RpcServiceT>::MethodResponse;
    type NotificationResponse = <RpcService as RpcServiceT>::NotificationResponse;
    type BatchResponse = <RpcService as RpcServiceT>::BatchResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let method = request.method_name().to_owned();
        let request_size = request.params.as_ref().map_or(0, |p| p.get().len());
        let inner = self.inner.clone();

        async move {
            let id = request.id.clone().into_owned();
            let handler = async move { inner.call(request).await };
            let on_panic = || MethodResponse::error(id, internal_rpc_err("Internal error"));
            CallGuard::new(CallKind::Call, method, request_size)
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

            for batch_entry in batch.into_iter() {
                match batch_entry {
                    Ok(BatchEntry::Call(req)) => {
                        let rp = service.call(req).await;
                        if let Err(err) = batch_rp.append(rp) {
                            return err;
                        }
                    }
                    Ok(BatchEntry::Notification(n)) => {
                        got_notification = true;
                        service.notification(n).await;
                    }
                    Err(err) => {
                        let (err, id) = err.into_parts();
                        let rp = MethodResponse::error(id, err);
                        if let Err(err) = batch_rp.append(rp) {
                            return err;
                        }
                    }
                }
            }

            // If the batch is empty, and we got a notification, we return an empty response.
            let response = if batch_rp.is_empty() && got_notification {
                MethodResponse::notification()
            } else {
                MethodResponse::from_batch(batch_rp.finish())
            };

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

        async move {
            let handler = async move { inner.notification(n).await };
            CallGuard::new(CallKind::Notification, method, request_size)
                .handle_result(handler, MethodResponse::notification)
                .await
        }
    }
}
