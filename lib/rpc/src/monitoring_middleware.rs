use crate::metrics::API_METRICS;
use jsonrpsee::core::middleware::{Batch, BatchEntry, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::{BatchResponseBuilder, MethodResponse};
use std::time::{Duration, Instant};

#[derive(Debug)]
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
        let fut = self.inner.call(request);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(
                CallKind::Call,
                &method,
                started.elapsed(),
                request_size,
                output_size,
            );
            out
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        // Collect some metrics about the batch
        let batch_size = batch.len();
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
            .fold(std::collections::HashMap::new(), |mut acc, method| {
                *acc.entry(method).or_insert(0) += 1;
                acc
            });

        let mut batch_rp = BatchResponseBuilder::new_with_limit(self.max_response_size_bytes);
        let service = self.clone();
        async move {
            let started = Instant::now();
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

            let response_size = response.as_json().get().len();
            let elapsed = started.elapsed();

            // Report batch metrics
            API_METRICS.response_time["batch"].observe(elapsed);
            API_METRICS.request_size["batch"].observe(batch_input_size);
            API_METRICS.response_size["batch"].observe(response_size);
            for (method, count) in request_counts {
                API_METRICS.requests_in_batch_count[&method].observe(count);
            }

            tracing::debug!(
                target: "rpc::monitoring::batch",
                batch_size,
                elapsed = ?elapsed,
                batch_input_size,
                response_size,
                "rpc batch call completed"
            );

            response
        }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let request_size = n.params.as_ref().map_or(0, |p| p.get().len());
        let method = n.method_name().to_owned();
        let fut = self.inner.notification(n);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(
                CallKind::Notification,
                &method,
                started.elapsed(),
                request_size,
                output_size,
            );
            out
        }
    }
}

/// Macro to statically dispatch debug logs to different targets based on the method name.
macro_rules! debug_dispatch {
    (
        targets: match $method:ident { $($method_arm:literal => $target_arm:literal,)* _ => $fallback:literal },
        fields: $fields:tt,
        message: $message:literal,
    ) => {
        match $method {
            $($method_arm => {
                tracing::debug!(
                    target: $target_arm,
                    $fields,
                    $message
                );
            })*
            _ => {
                tracing::debug!(
                    target: $fallback,
                    $fields,
                    $message
                );
            }
        }
    };
}

fn log_and_report(
    kind: CallKind,
    method: &str,
    elapsed: Duration,
    request_size: usize,
    output_size_bytes: usize,
) {
    API_METRICS.response_time[method].observe(elapsed);
    API_METRICS.request_size[method].observe(request_size);
    API_METRICS.response_size[method].observe(output_size_bytes);

    debug_dispatch!(
        targets: match method {
            "eth_call" => "rpc::monitoring::eth::call",
            "eth_sendRawTransaction" => "rpc::monitoring::eth::sendRawTransaction",
            "debug_traceTransaction" => "rpc::monitoring::debug::traceTransaction",
            _ => "rpc::monitoring::call"
        },
        fields: {
            method,
            ?kind,
            ?elapsed,
            request_size,
            output_size_bytes,
        },
        message: "rpc call completed",
    );
}
