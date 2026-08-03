use crate::metrics::{API_METRICS, RPC_TASK_MONITOR};
use crate::result::internal_rpc_err;
use futures::{FutureExt as _, StreamExt as _};
use jsonrpsee::core::middleware::{Batch, BatchEntry, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::error::reject_too_big_batch_response;
use jsonrpsee::types::{Id, Request};
use jsonrpsee::{BatchResponseBuilder, MethodResponse};
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::task::AbortOnDropHandle;

#[derive(Clone, Copy, Debug)]
pub enum CallKind {
    Call,
    Notification,
}

/// Metric label for any method name that isn't registered on the server. Folding all such names
/// into one label bounds metric cardinality, so a client can't spawn unbounded time series by
/// sending requests for arbitrary nonexistent methods.
const UNKNOWN_METHOD: &str = "<unknown>";

/// Keeps one batch from flooding the runtime with spawned tasks. This limit applies per batch;
/// separate batches and connections can still make progress independently.
const MAX_CONCURRENT_BATCH_ENTRIES: usize = 32;

/// Records RPC metrics and owns the custom batch-dispatch behavior.
///
/// Calls always pass through the inner service. When parallel batches are enabled, calls within a
/// batch are spawned with bounded concurrency and their responses are put back in request order.
#[derive(Clone)]
pub struct Monitoring<S = RpcService> {
    inner: S,
    max_response_size_bytes: usize,
    known_methods: Arc<HashSet<&'static str>>,
    parallel_batches: bool,
}

impl<S> Monitoring<S> {
    pub fn new(
        inner: S,
        max_response_size_bytes: u32,
        known_methods: Arc<HashSet<&'static str>>,
        parallel_batches: bool,
    ) -> Self {
        Self {
            inner,
            max_response_size_bytes: max_response_size_bytes as usize,
            known_methods,
            parallel_batches,
        }
    }
}

/// Maps a method name to a bounded metric label: the registered name (a `'static` string, so no
/// per-request allocation) or [`UNKNOWN_METHOD`].
fn method_label(known_methods: &HashSet<&'static str>, method: &str) -> &'static str {
    known_methods.get(method).copied().unwrap_or(UNKNOWN_METHOD)
}

/// Ensures latency is recorded even if the future is dropped mid-flight (client disconnected).
struct CallGuard {
    kind: CallKind,
    method: &'static str,
    started: Instant,
    request_size: usize,
    /// `Some((output_size, error_code))` once the future has resolved.
    completed: Option<(usize, Option<i32>)>,
    panicked: bool,
}

impl CallGuard {
    fn new(kind: CallKind, method: &'static str, request_size: usize) -> Self {
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
    request_counts: HashMap<&'static str, u64>,
    started: Instant,
    /// `Some(response_size)` once the batch has resolved.
    completed: Option<usize>,
}

impl BatchGuard {
    fn new(batch_input_size: usize, request_counts: HashMap<&'static str, u64>) -> Self {
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
            API_METRICS.requests_in_batch_count[*method].observe(*count);
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
        API_METRICS.response_time[self.method].observe(elapsed);
        API_METRICS.request_size[self.method].observe(self.request_size);
        API_METRICS.response_size[self.method].observe(output_size);
        if let Some(code) = error_code {
            API_METRICS.errors[&(self.method.to_owned(), code)].inc();
        }
        if cancelled {
            API_METRICS.cancelled[self.method].inc();
        }
        if self.panicked {
            API_METRICS.panicked[self.method].inc();
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

        match self.method {
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
        + Sync
        + 'static,
{
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;
    type BatchResponse = MethodResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let method = method_label(&self.known_methods, request.method_name());
        let request_size = request.params.as_ref().map_or(0, |p| p.get().len());
        let inner = self.inner.clone();

        async move {
            let id = request.id.clone();
            let handler = RPC_TASK_MONITOR.instrument(async move { inner.call(request).await });
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
                    Some(method_label(&self.known_methods, req.method_name()))
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

            if service.parallel_batches {
                let responses = match service
                    .execute_batch_parallel(batch, &mut got_notification)
                    .await
                {
                    Ok(responses) => responses,
                    Err(err) => return err,
                };
                for rp in responses {
                    if let Err(err) = batch_rp.append(rp) {
                        return err;
                    }
                }
            } else {
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
        let method = method_label(&self.known_methods, n.method_name());
        let inner = self.inner.clone();

        async move {
            let handler = async move { inner.notification(n).await };
            CallGuard::new(CallKind::Notification, method, request_size)
                .handle_result(handler, MethodResponse::notification)
                .await
        }
    }
}

impl<S> Monitoring<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            NotificationResponse = MethodResponse,
            BatchResponse = MethodResponse,
        > + Clone
        + Send
        + Sync
        + 'static,
{
    /// Executes a batch's calls in parallel, with at most [`MAX_CONCURRENT_BATCH_ENTRIES`] in
    /// flight, and returns their responses in request order. Notifications are handled inline
    /// (they have no response slot) and reported via `got_notification`.
    ///
    /// Fails with the whole-batch error response when the combined response size exceeds
    /// `max_response_size_bytes`.
    async fn execute_batch_parallel(
        &self,
        batch: Batch<'_>,
        got_notification: &mut bool,
    ) -> Result<Vec<MethodResponse>, MethodResponse> {
        enum Prepared {
            /// An owned call ready to spawn. Its id is used if the task cannot be joined.
            Call {
                id: Id<'static>,
                req: Request<'static>,
            },
            /// A malformed entry whose error response is already known.
            Ready(MethodResponse),
        }

        let mut prepared = Vec::with_capacity(batch.len());
        for batch_entry in batch.into_iter() {
            match batch_entry {
                Ok(BatchEntry::Call(mut req)) => {
                    let id = req.id.clone().into_owned();
                    // Spawned tasks require `'static` data. Rebuild the request from owned
                    // parts, including the per-connection context in its extensions.
                    let mut owned = Request::owned(
                        req.method_name().to_owned(),
                        req.params.take().map(|params| params.into_owned()),
                        id.clone(),
                    );
                    *owned.extensions_mut() = std::mem::take(req.extensions_mut());
                    prepared.push(Prepared::Call { id, req: owned });
                }
                Ok(BatchEntry::Notification(n)) => {
                    *got_notification = true;
                    // jsonrpsee's root service only propagates notification extensions;
                    // awaiting it here records our metrics without making `Prepared`
                    // borrow from the batch.
                    self.notification(n).await;
                }
                Err(err) => {
                    let (err, id) = err.into_parts();
                    prepared.push(Prepared::Ready(MethodResponse::error(id, err)));
                }
            }
        }

        // Spawning lets CPU-heavy handlers use multiple runtime workers. An unordered
        // buffer keeps the window full when an early call is slow; sorting below restores
        // request order before the response is built.
        //
        // Abort-on-drop prevents calls from running detached after the batch is cancelled.
        // Cancellation takes effect when a handler next yields.
        let response_capacity = prepared.len();
        let mut response_stream = futures::stream::iter(prepared.into_iter().enumerate().map(
            move |(index, entry)| async move {
                let rp = match entry {
                    Prepared::Call { id, req } => {
                        match AbortOnDropHandle::new(tokio::spawn(self.call(req))).await {
                            Ok(rp) => rp,
                            // `CallGuard` converts handler panics. Treat any remaining
                            // task failure as an internal error for this call.
                            Err(_) => MethodResponse::error(id, internal_rpc_err("Internal error")),
                        }
                    }
                    Prepared::Ready(rp) => rp,
                };
                (index, rp)
            },
        ))
        .buffer_unordered(MAX_CONCURRENT_BATCH_ENTRIES);

        // Enforce the limit as unordered results arrive. This caps retained responses and
        // dropping the stream on overflow cancels work whose output would be discarded.
        // The accounting mirrors `BatchResponseBuilder`: `[` plus every response and one
        // delimiter, with the final comma replaced by `]`.
        let mut responses = Vec::with_capacity(response_capacity);
        let mut response_size = 1usize;
        while let Some((index, rp)) = response_stream.next().await {
            let next_size = response_size
                .checked_add(rp.as_json().get().len())
                .and_then(|size| size.checked_add(1));
            match next_size {
                Some(next_size) if next_size <= self.max_response_size_bytes => {
                    response_size = next_size;
                    responses.push((index, rp));
                }
                _ => {
                    return Err(MethodResponse::error(
                        Id::Null,
                        reject_too_big_batch_response(self.max_response_size_bytes),
                    ));
                }
            }
        }

        responses.sort_unstable_by_key(|(index, _)| *index);
        Ok(responses.into_iter().map(|(_, rp)| rp).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::ResponsePayload;
    use jsonrpsee::core::middleware::{BatchEntryErr, RpcServiceBuilder};
    use jsonrpsee::types::ErrorObject;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn registered_methods_pass_through_unknown_methods_collapse() {
        let known: HashSet<&'static str> = ["eth_call", "eth_getBlockByHash"].into_iter().collect();

        // Registered methods are reported verbatim.
        assert_eq!(method_label(&known, "eth_call"), "eth_call");
        assert_eq!(
            method_label(&known, "eth_getBlockByHash"),
            "eth_getBlockByHash"
        );

        // Anything unregistered — including arbitrarily long junk used to pollute metrics —
        // collapses to a single bounded label instead of minting a new time series.
        assert_eq!(method_label(&known, "eth_does_not_exist"), UNKNOWN_METHOD);
        assert_eq!(method_label(&known, ""), UNKNOWN_METHOD);
        let junk = format!("eth_{}", "a".repeat(1_000_000));
        assert_eq!(method_label(&known, &junk), UNKNOWN_METHOD);
    }

    /// Marker inserted into request extensions to prove they survive the owned-request rebuild
    /// on the parallel path.
    #[derive(Clone)]
    struct Marker;

    /// Decrements the in-flight counter on drop, so cancelled/aborted entries are accounted
    /// for and not just ones that ran to completion.
    struct InFlightGuard(Arc<AtomicUsize>);

    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Stands in for the rest of the middleware stack. Behavior is keyed off the method name:
    /// `sleep_<ms>` awaits a timer, `block_<ms>` blocks its OS thread, `wait_for_<n>` waits
    /// until `n` entries have completed, `payload_<bytes>` produces a result of that size,
    /// `panic` panics, and anything else responds immediately.
    #[derive(Clone, Default)]
    struct MockService {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
        notifications: Arc<AtomicUsize>,
        threads_seen: Arc<Mutex<HashSet<std::thread::ThreadId>>>,
    }

    impl RpcServiceT for MockService {
        type MethodResponse = MethodResponse;
        type NotificationResponse = MethodResponse;
        type BatchResponse = MethodResponse;

        fn call<'a>(
            &self,
            request: Request<'a>,
        ) -> impl Future<Output = MethodResponse> + Send + 'a {
            let this = self.clone();
            async move {
                let n = this.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                this.max_in_flight.fetch_max(n, Ordering::SeqCst);
                let _in_flight = InFlightGuard(this.in_flight.clone());
                this.threads_seen
                    .lock()
                    .unwrap()
                    .insert(std::thread::current().id());

                let method = request.method_name().to_owned();
                let id = request.id.clone().into_owned();
                let has_marker = request.extensions().get::<Marker>().is_some();

                if method == "panic" {
                    panic!("mock method panicked");
                }
                if let Some(ms) = method.strip_prefix("sleep_") {
                    tokio::time::sleep(Duration::from_millis(ms.parse().unwrap())).await;
                }
                if let Some(ms) = method.strip_prefix("block_") {
                    std::thread::sleep(Duration::from_millis(ms.parse().unwrap()));
                }
                if let Some(count) = method.strip_prefix("wait_for_") {
                    let count: usize = count.parse().unwrap();
                    while this.completed.load(Ordering::SeqCst) < count {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }

                let result = if let Some(bytes) = method.strip_prefix("payload_") {
                    "x".repeat(bytes.parse().unwrap())
                } else {
                    format!("{method}:{has_marker}")
                };
                this.completed.fetch_add(1, Ordering::SeqCst);
                MethodResponse::response(id, ResponsePayload::success(result), usize::MAX)
            }
        }

        // `async fn` would tie the returned future to `&self`'s lifetime and fail the trait's
        // `+ 'a` bound, so the manual form is required despite the lint.
        #[allow(clippy::manual_async_fn)]
        fn batch<'a>(&self, _batch: Batch<'a>) -> impl Future<Output = MethodResponse> + Send + 'a {
            async move { unreachable!("Monitoring::batch dispatches entries via `call`") }
        }

        fn notification<'a>(
            &self,
            _n: Notification<'a>,
        ) -> impl Future<Output = MethodResponse> + Send + 'a {
            let this = self.clone();
            async move {
                this.notifications.fetch_add(1, Ordering::SeqCst);
                MethodResponse::notification()
            }
        }
    }

    fn monitoring_with_max_response_size(
        parallel_batches: bool,
        max_response_size: u32,
    ) -> (Monitoring<MockService>, MockService) {
        let mock = MockService::default();
        let monitoring = Monitoring::new(
            mock.clone(),
            max_response_size,
            Arc::new(HashSet::new()),
            parallel_batches,
        );
        (monitoring, mock)
    }

    fn monitoring(parallel_batches: bool) -> (Monitoring<MockService>, MockService) {
        monitoring_with_max_response_size(parallel_batches, 10 * 1024 * 1024)
    }

    async fn assert_no_in_flight_entries(mock: &MockService) {
        // Aborts propagate asynchronously; wait for the in-flight count to drain.
        for _ in 0..100 {
            if mock.in_flight.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            mock.in_flight.load(Ordering::SeqCst),
            0,
            "spawned entries kept running after cancellation was requested"
        );
    }

    fn call_entry(id: u64, method: &str) -> Result<BatchEntry<'static>, BatchEntryErr<'static>> {
        Ok(BatchEntry::Call(Request::owned(
            method.to_owned(),
            None,
            Id::Number(id),
        )))
    }

    fn notification_entry() -> Result<BatchEntry<'static>, BatchEntryErr<'static>> {
        Ok(BatchEntry::Notification(Notification::new(
            "notify".into(),
            None,
        )))
    }

    fn parse_batch(rp: &MethodResponse) -> Vec<(u64, Result<String, i64>)> {
        let json: serde_json::Value = serde_json::from_str(rp.as_json().get()).unwrap();
        json.as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let id = entry["id"].as_u64().unwrap();
                match entry.get("result") {
                    Some(result) => (id, Ok(result.as_str().unwrap().to_owned())),
                    None => (id, Err(entry["error"]["code"].as_i64().unwrap())),
                }
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_batch_preserves_request_order() {
        let (monitoring, _) = monitoring(true);
        // Reverse-staggered delays: the first entry is the slowest, so completion order is the
        // reverse of request order unless responses are explicitly kept in request order.
        let entries = (0..8u64)
            .map(|i| call_entry(i, &format!("sleep_{}", (8 - i) * 20)))
            .collect();

        let rp = monitoring.batch(Batch::from(entries)).await;

        let parsed = parse_batch(&rp);
        let ids: Vec<u64> = parsed.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
        assert!(parsed.iter().all(|(_, result)| result.is_ok()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_batch_keeps_malformed_entries_in_their_slots() {
        let (monitoring, _) = monitoring(true);
        let entries = vec![
            call_entry(0, "echo"),
            Err(BatchEntryErr::new(
                Id::Number(1),
                ErrorObject::owned(-32600, "Invalid Request", None::<()>),
            )),
            call_entry(2, "echo"),
        ];

        let rp = monitoring.batch(Batch::from(entries)).await;

        let parsed = parse_batch(&rp);
        assert_eq!(parsed[0], (0, Ok("echo:false".to_owned())));
        assert_eq!(parsed[1], (1, Err(-32600)));
        assert_eq!(parsed[2], (2, Ok("echo:false".to_owned())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_batch_bounds_concurrency() {
        let (monitoring, mock) = monitoring(true);
        let total = 4 * MAX_CONCURRENT_BATCH_ENTRIES as u64;
        let entries = (0..total).map(|i| call_entry(i, "sleep_20")).collect();

        let rp = monitoring.batch(Batch::from(entries)).await;

        assert_eq!(parse_batch(&rp).len(), total as usize);
        let max = mock.max_in_flight.load(Ordering::SeqCst);
        assert!(
            max <= MAX_CONCURRENT_BATCH_ENTRIES,
            "in-flight entries exceeded the bound: {max}"
        );
        assert!(max >= 2, "entries never overlapped: max in-flight {max}");
    }

    /// The first entry waits for 40 later entries. An ordered buffer stops refilling behind that
    /// entry and deadlocks; unordered buffering lets later entries start and release it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_batch_replenishes_window_behind_slow_entry() {
        let (monitoring, _) = monitoring(true);
        let mut entries = vec![call_entry(0, "wait_for_40")];
        entries.extend((1..65u64).map(|i| call_entry(i, "sleep_1")));

        let rp = tokio::time::timeout(
            Duration::from_secs(10),
            monitoring.batch(Batch::from(entries)),
        )
        .await
        .expect("deadlocked: window did not replenish behind the slow first entry");

        let ids: Vec<u64> = parse_batch(&rp).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, (0..65).collect::<Vec<_>>());
    }

    /// Dropping a batch must abort its spawned entries, so a disconnected client leaves no
    /// detached work behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn cancelling_parallel_batch_aborts_spawned_entries() {
        let (monitoring, mock) = monitoring(true);
        let entries = (0..16u64).map(|i| call_entry(i, "sleep_10000")).collect();

        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            monitoring.batch(Batch::from(entries)),
        )
        .await;
        assert!(cancelled.is_err(), "batch must still be sleeping at 100ms");
        assert!(mock.max_in_flight.load(Ordering::SeqCst) > 0);

        assert_no_in_flight_entries(&mock).await;
        assert_eq!(mock.completed.load(Ordering::SeqCst), 0);
    }

    /// Aggregate response overflow must be detected while results arrive so a batch cannot
    /// retain every individually valid response or keep executing work whose output is already
    /// known to be unusable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn oversized_parallel_batch_aborts_remaining_entries() {
        let (monitoring, mock) = monitoring_with_max_response_size(true, 512);
        let mut entries: Vec<_> = (0..30u64).map(|i| call_entry(i, "sleep_10000")).collect();
        entries.push(call_entry(30, "payload_300"));
        entries.push(call_entry(31, "payload_300"));

        let rp = tokio::time::timeout(
            Duration::from_secs(1),
            monitoring.batch(Batch::from(entries)),
        )
        .await
        .expect("aggregate response limit was not enforced before slow entries completed");

        assert_eq!(
            rp.as_error_code(),
            Some(jsonrpsee::types::error::TOO_BIG_BATCH_RESPONSE_CODE)
        );
        assert!(mock.max_in_flight.load(Ordering::SeqCst) > 2);
        assert_no_in_flight_entries(&mock).await;
        assert_eq!(mock.completed.load(Ordering::SeqCst), 2);
    }

    /// The point of spawning entries (rather than just polling them concurrently on one task):
    /// work that occupies its thread spreads across runtime workers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_batch_spreads_entries_across_threads() {
        let (monitoring, mock) = monitoring(true);
        let entries = (0..8u64).map(|i| call_entry(i, "block_50")).collect();

        monitoring.batch(Batch::from(entries)).await;

        let threads = mock.threads_seen.lock().unwrap().len();
        assert!(
            threads >= 2,
            "all thread-blocking entries ran on a single worker thread"
        );
    }

    /// Slot bookkeeping survives windowing: a batch much larger than the concurrency bound,
    /// mixing calls, malformed entries and notifications, comes back with exactly the
    /// non-notification entries in request order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn large_heterogeneous_parallel_batch_keeps_slots() {
        let (monitoring, mock) = monitoring(true);
        let mut entries = Vec::new();
        let mut expected = Vec::new();
        let mut notifications = 0;
        for i in 0..80u64 {
            if i % 7 == 3 {
                entries.push(Err(BatchEntryErr::new(
                    Id::Number(i),
                    ErrorObject::owned(-32600, "Invalid Request", None::<()>),
                )));
                expected.push((i, Err(-32600)));
            } else if i % 11 == 5 {
                entries.push(notification_entry());
                notifications += 1;
            } else if i % 2 == 0 {
                entries.push(call_entry(i, "echo"));
                expected.push((i, Ok("echo:false".to_owned())));
            } else {
                entries.push(call_entry(i, "sleep_2"));
                expected.push((i, Ok("sleep_2:false".to_owned())));
            }
        }

        let rp = monitoring.batch(Batch::from(entries)).await;

        assert_eq!(parse_batch(&rp), expected);
        assert_eq!(mock.notifications.load(Ordering::SeqCst), notifications);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sequential_batch_runs_entries_one_at_a_time() {
        let (monitoring, mock) = monitoring(false);
        let entries = (0..10u64).map(|i| call_entry(i, "sleep_5")).collect();

        let rp = monitoring.batch(Batch::from(entries)).await;

        let ids: Vec<u64> = parse_batch(&rp).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, (0..10).collect::<Vec<_>>());
        assert_eq!(mock.max_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn panicking_entry_fails_alone_in_parallel_batch() {
        let (monitoring, _) = monitoring(true);
        let entries = vec![
            call_entry(0, "echo"),
            call_entry(1, "panic"),
            call_entry(2, "echo"),
        ];

        let rp = monitoring.batch(Batch::from(entries)).await;

        let parsed = parse_batch(&rp);
        assert_eq!(parsed[0], (0, Ok("echo:false".to_owned())));
        // `CallGuard` converts the panic into an internal error for that slot only.
        assert_eq!(parsed[1], (1, Err(-32603)));
        assert_eq!(parsed[2], (2, Ok("echo:false".to_owned())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_batch_carries_extensions_into_spawned_calls() {
        let (monitoring, _) = monitoring(true);
        let entries = (0..3u64)
            .map(|i| {
                let mut req = Request::owned("echo".to_owned(), None, Id::Number(i));
                req.extensions_mut().insert(Marker);
                Ok(BatchEntry::Call(req))
            })
            .collect();

        let rp = monitoring.batch(Batch::from(entries)).await;

        for (_, result) in parse_batch(&rp) {
            assert_eq!(result, Ok("echo:true".to_owned()));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notifications_in_parallel_batch_have_no_response_slot() {
        let (monitoring, mock) = monitoring(true);
        let entries = vec![
            call_entry(0, "echo"),
            notification_entry(),
            call_entry(2, "echo"),
        ];

        let rp = monitoring.batch(Batch::from(entries)).await;

        let ids: Vec<u64> = parse_batch(&rp).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![0, 2]);
        assert_eq!(mock.notifications.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_notification_parallel_batch_returns_notification_response() {
        let (monitoring, mock) = monitoring(true);
        let entries = vec![notification_entry(), notification_entry()];

        let rp = monitoring.batch(Batch::from(entries)).await;

        assert!(rp.is_notification());
        assert_eq!(mock.notifications.load(Ordering::SeqCst), 2);
    }

    /// Real HTTP reaches jsonrpsee's rejection of subscription calls without a WebSocket sink.
    /// Response order is covered by the mock tests because the HTTP client re-correlates entries
    /// by id.
    mod http {
        use super::*;
        use jsonrpsee::RpcModule;
        use jsonrpsee::core::client::ClientT;
        use jsonrpsee::core::params::BatchRequestBuilder;
        use jsonrpsee::http_client::HttpClientBuilder;
        use jsonrpsee::server::{ServerBuilder, ServerHandle};

        async fn spin_server() -> (ServerHandle, String) {
            let mut module = RpcModule::new(());
            module
                .register_async_method("say", |params, _ctx, _ext| async move {
                    let (tag,): (String,) = params.parse().unwrap();
                    tag
                })
                .unwrap();
            module
                .register_subscription(
                    "sub",
                    "sub_note",
                    "unsub",
                    |_params, _pending, _ctx, _ext| async {},
                )
                .unwrap();

            let known_methods = Arc::new(module.method_names().collect::<HashSet<&'static str>>());
            let rpc_middleware = RpcServiceBuilder::new().layer_fn(move |service| {
                Monitoring::new(service, 10 * 1024 * 1024, known_methods.clone(), true)
            });
            let server = ServerBuilder::default()
                .set_rpc_middleware(rpc_middleware)
                .build("127.0.0.1:0")
                .await
                .unwrap();
            let addr = server.local_addr().unwrap();
            (server.start(module), format!("http://{addr}"))
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn subscription_in_http_batch_fails_alone() {
            let (_handle, url) = spin_server().await;
            let client = HttpClientBuilder::default().build(url).unwrap();

            let mut batch = BatchRequestBuilder::new();
            batch
                .insert("say", jsonrpsee::rpc_params!["first"])
                .unwrap();
            batch.insert("sub", jsonrpsee::rpc_params![]).unwrap();
            batch.insert("say", jsonrpsee::rpc_params!["last"]).unwrap();

            let entries: Vec<_> = client
                .batch_request::<serde_json::Value>(batch)
                .await
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(entries[0].as_ref().unwrap(), "first");
            assert!(entries[1].is_err());
            assert_eq!(entries[2].as_ref().unwrap(), "last");
        }
    }
}
