use crate::metrics::{API_METRICS, RPC_TASK_MONITOR};
use crate::result::internal_rpc_err;
use futures::{FutureExt as _, StreamExt as _};
use jsonrpsee::core::middleware::{Batch, BatchEntry, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::{Id, Request};
use jsonrpsee::{BatchResponseBuilder, MethodResponse};
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum CallKind {
    Call,
    Notification,
}

/// Metric label for any method name that isn't registered on the server. Folding all such names
/// into one label bounds metric cardinality, so a client can't spawn unbounded time series by
/// sending requests for arbitrary nonexistent methods.
const UNKNOWN_METHOD: &str = "<unknown>";

/// Upper bound on how many entries of a single batch may execute concurrently when
/// `parallel_batches` is enabled. Without it one huge batch would spawn a task per entry and
/// could monopolize every runtime worker; parallelism across separate batches/connections is
/// unaffected by this bound.
const MAX_CONCURRENT_BATCH_ENTRIES: usize = 32;

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
            let id = request.id.clone().into_owned();
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
                enum Prepared {
                    /// A call rebuilt as owned so it can be spawned; the id is kept separately
                    /// to report a join error in the right slot.
                    Call {
                        id: Id<'static>,
                        req: Request<'static>,
                    },
                    /// Response already known (malformed entry); keeps its slot in the array.
                    Ready(MethodResponse),
                }

                let mut prepared = Vec::with_capacity(batch.len());
                for batch_entry in batch.into_iter() {
                    match batch_entry {
                        Ok(BatchEntry::Call(mut req)) => {
                            let id = req.id.clone().into_owned();
                            // Spawning requires a `'static` request: rebuild it from owned
                            // parts and carry the extensions over (the server clones the
                            // per-connection context into every batch entry).
                            let mut owned = Request::owned(
                                req.method_name().to_owned(),
                                req.params.take().map(|params| params.into_owned()),
                                id.clone(),
                            );
                            *owned.extensions_mut() = std::mem::take(req.extensions_mut());
                            prepared.push(Prepared::Call { id, req: owned });
                        }
                        Ok(BatchEntry::Notification(n)) => {
                            got_notification = true;
                            // Notifications have no response slot and no ordering requirement;
                            // handle them inline like the sequential path does.
                            service.notification(n).await;
                        }
                        Err(err) => {
                            let (err, id) = err.into_parts();
                            prepared.push(Prepared::Ready(MethodResponse::error(id, err)));
                        }
                    }
                }

                // Entries execute concurrently so CPU-heavy handlers (e.g.
                // `eth_sendRawTransaction`'s inline ECDSA recovery) spread across worker
                // threads instead of serializing on this connection's task. `buffered` starts
                // at most MAX_CONCURRENT_BATCH_ENTRIES wrappers at a time and yields results
                // in request order, which the JSON-RPC response array must follow.
                //
                // The closure owns its own service handle (instead of borrowing the outer one)
                // to keep the stream `Send` without requiring `S: Sync`.
                let svc = service.clone();
                let mut responses = futures::stream::iter(prepared.into_iter().map(move |entry| {
                    let service = svc.clone();
                    async move {
                        match entry {
                            Prepared::Call { id, req } => {
                                match tokio::spawn(service.call(req)).await {
                                    Ok(rp) => rp,
                                    // Panics are caught inside the spawned future by
                                    // `CallGuard`; a join error only means the task was
                                    // aborted (runtime shutdown).
                                    Err(_) => MethodResponse::error(
                                        id,
                                        internal_rpc_err("Internal error"),
                                    ),
                                }
                            }
                            Prepared::Ready(rp) => rp,
                        }
                    }
                }))
                .buffered(MAX_CONCURRENT_BATCH_ENTRIES);

                while let Some(rp) = responses.next().await {
                    if let Err(err) = batch_rp.append(rp) {
                        // Entries still in flight keep running detached and their side effects
                        // may still land even though the whole batch is replaced by this error
                        // response.
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

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::ResponsePayload;
    use jsonrpsee::core::middleware::{BatchEntryErr, RpcServiceBuilder};
    use jsonrpsee::types::ErrorObject;
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

    /// Stands in for the rest of the middleware stack. Behavior is keyed off the method name:
    /// `sleep_<ms>` sleeps before responding, `panic` panics, anything else responds
    /// immediately. Every response payload is `"<method>:<has extensions marker>"`.
    #[derive(Clone, Default)]
    struct MockService {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        notifications: Arc<AtomicUsize>,
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

                let method = request.method_name().to_owned();
                let id = request.id.clone().into_owned();
                let has_marker = request.extensions().get::<Marker>().is_some();

                if method == "panic" {
                    // `in_flight` intentionally stays incremented; panicking tests don't
                    // assert on it.
                    panic!("mock method panicked");
                }
                if let Some(ms) = method.strip_prefix("sleep_") {
                    tokio::time::sleep(Duration::from_millis(ms.parse().unwrap())).await;
                }

                this.in_flight.fetch_sub(1, Ordering::SeqCst);
                MethodResponse::response(
                    id,
                    ResponsePayload::success(format!("{method}:{has_marker}")),
                    usize::MAX,
                )
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

    fn monitoring(parallel_batches: bool) -> (Monitoring<MockService>, MockService) {
        let mock = MockService::default();
        let monitoring = Monitoring::new(
            mock.clone(),
            10 * 1024 * 1024,
            Arc::new(HashSet::new()),
            parallel_batches,
        );
        (monitoring, mock)
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

    /// Parses a batch response into `(id, Ok(result) | Err(error code))` tuples in array order.
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

    /// End-to-end tests over real HTTP: the full jsonrpsee server stack parses the batch, so
    /// these cover transport-level behavior the mock tests can't (extensions injection,
    /// subscription rejection, the batch size limit).
    mod http {
        use super::*;
        use jsonrpsee::RpcModule;
        use jsonrpsee::core::client::ClientT;
        use jsonrpsee::core::params::BatchRequestBuilder;
        use jsonrpsee::http_client::HttpClientBuilder;
        use jsonrpsee::server::{
            BatchRequestConfig, ServerBuilder, ServerConfigBuilder, ServerHandle,
        };

        async fn spin_server(batch_config: BatchRequestConfig) -> (ServerHandle, String) {
            let mut module = RpcModule::new(());
            module
                .register_async_method("say", |params, _ctx, _ext| async move {
                    let (delay_ms, tag): (u64, String) = params.parse().unwrap();
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
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
            let server_config = ServerConfigBuilder::default()
                .set_batch_request_config(batch_config)
                .build();
            let server = ServerBuilder::default()
                .set_config(server_config)
                .set_rpc_middleware(rpc_middleware)
                .build("127.0.0.1:0")
                .await
                .unwrap();
            let addr = server.local_addr().unwrap();
            (server.start(module), format!("http://{addr}"))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn http_batch_responses_arrive_in_request_order() {
            let (_handle, url) = spin_server(BatchRequestConfig::Unlimited).await;
            let client = HttpClientBuilder::default().build(url).unwrap();

            let mut batch = BatchRequestBuilder::new();
            for i in 0..8u64 {
                // Reverse-staggered delays, as in the mock ordering test.
                batch
                    .insert("say", jsonrpsee::rpc_params![(8 - i) * 20, i.to_string()])
                    .unwrap();
            }

            let tags: Vec<String> = client
                .batch_request::<String>(batch)
                .await
                .unwrap()
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(tags, (0..8).map(|i| i.to_string()).collect::<Vec<_>>());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn subscription_in_http_batch_fails_alone() {
            let (_handle, url) = spin_server(BatchRequestConfig::Unlimited).await;
            let client = HttpClientBuilder::default().build(url).unwrap();

            let mut batch = BatchRequestBuilder::new();
            batch
                .insert("say", jsonrpsee::rpc_params![0u64, "first"])
                .unwrap();
            batch.insert("sub", jsonrpsee::rpc_params![]).unwrap();
            batch
                .insert("say", jsonrpsee::rpc_params![0u64, "last"])
                .unwrap();

            let entries: Vec<_> = client
                .batch_request::<serde_json::Value>(batch)
                .await
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(entries[0].as_ref().unwrap(), "first");
            // Subscriptions need a ws sink; over HTTP jsonrpsee rejects the entry itself while
            // the rest of the batch still executes.
            assert!(entries[1].is_err());
            assert_eq!(entries[2].as_ref().unwrap(), "last");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn over_limit_http_batch_is_rejected_whole() {
            let (_handle, url) = spin_server(BatchRequestConfig::Limit(4)).await;
            let client = HttpClientBuilder::default().build(url).unwrap();

            let batch_of = |n: u64| {
                let mut batch = BatchRequestBuilder::new();
                for i in 0..n {
                    batch
                        .insert("say", jsonrpsee::rpc_params![0u64, i.to_string()])
                        .unwrap();
                }
                batch
            };

            // At the limit the batch executes normally...
            let ok = client.batch_request::<String>(batch_of(4)).await.unwrap();
            assert_eq!(ok.num_successful_calls(), 4);

            // ...one entry above it, the whole batch is rejected before any entry runs: the
            // server answers with a single error object instead of a response array, which
            // the jsonrpsee client surfaces as a parse error.
            let err = client
                .batch_request::<String>(batch_of(5))
                .await
                .expect_err("batch above the configured limit must be rejected");
            assert!(
                matches!(err, jsonrpsee::core::ClientError::ParseError(_)),
                "unexpected error: {err}"
            );
        }
    }
}
