use anyhow::Context as _;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct RpcRecordConfig {
    pub poll_interval: Duration,
    pub request_timeout: Duration,
    pub max_samples: usize,
}

impl Default for RpcRecordConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            request_timeout: Duration::from_secs(1),
            max_samples: 5_000,
        }
    }
}

#[derive(Debug)]
pub struct HttpRpcRecorder {
    name: String,
    url: String,
    started_at: Instant,
    shared: Arc<Mutex<Vec<HttpRpcSample>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct HttpRpcReport {
    pub name: String,
    pub url: String,
    pub started_at: Instant,
    pub finished_at: Instant,
    pub samples: Vec<HttpRpcSample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpRpcSample {
    pub elapsed: Duration,
    pub latency: Duration,
    pub chain_id: Option<String>,
    pub block_number: Option<u64>,
    pub latest_block_hash: Option<String>,
    pub status: HttpRpcSampleStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HttpRpcSampleStatus {
    Ready,
    TransportError { message: String },
    HttpError { status: u16, body: String },
    JsonRpcError { message: String },
    InvalidResponse { message: String },
}

impl HttpRpcSample {
    pub fn is_ready(&self) -> bool {
        matches!(self.status, HttpRpcSampleStatus::Ready)
    }
}

impl HttpRpcRecorder {
    pub fn start_http(
        name: impl Into<String>,
        url: impl Into<String>,
        config: RpcRecordConfig,
    ) -> Self {
        let name = name.into();
        let url = url.into();
        let started_at = Instant::now();
        let shared = Arc::new(Mutex::new(Vec::new()));
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task_shared = Arc::clone(&shared);
        let task_name = name.clone();
        let task_url = url.clone();
        let task = tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(config.request_timeout)
                .build()
            {
                Ok(client) => client,
                Err(err) => {
                    let mut samples = task_shared.lock().await;
                    samples.push(HttpRpcSample {
                        elapsed: started_at.elapsed(),
                        latency: Duration::ZERO,
                        chain_id: None,
                        block_number: None,
                        latest_block_hash: None,
                        status: HttpRpcSampleStatus::TransportError {
                            message: format!("failed to build HTTP client: {err}"),
                        },
                    });
                    return;
                }
            };

            let mut interval = tokio::time::interval(config.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            let initial_sample = sample_http_rpc(&client, started_at, &task_url).await;
            {
                let mut samples = task_shared.lock().await;
                samples.push(initial_sample);
            }

            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = interval.tick() => {
                        let sample = sample_http_rpc(&client, started_at, &task_url).await;
                        let mut samples = task_shared.lock().await;
                        samples.push(sample);
                        if samples.len() > config.max_samples {
                            let overflow = samples.len() - config.max_samples;
                            samples.drain(..overflow);
                        }
                    }
                }
            }

            let samples_len = task_shared.lock().await.len();
            tracing::debug!(
                recorder = %task_name,
                url = %task_url,
                samples = samples_len,
                "stopped HTTP RPC recorder"
            );
        });

        Self {
            name,
            url,
            started_at,
            shared,
            stop_tx: Some(stop_tx),
            task: Some(task),
        }
    }

    pub async fn stop(mut self) -> HttpRpcReport {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }

        let finished_at = Instant::now();
        let samples = self.shared.lock().await.clone();
        HttpRpcReport {
            name: self.name,
            url: self.url,
            started_at: self.started_at,
            finished_at,
            samples,
        }
    }
}

impl HttpRpcReport {
    pub fn total_duration(&self) -> Duration {
        self.finished_at.duration_since(self.started_at)
    }

    pub fn total_samples(&self) -> usize {
        self.samples.len()
    }

    pub fn ready_samples(&self) -> usize {
        self.samples
            .iter()
            .filter(|sample| sample.is_ready())
            .count()
    }

    pub fn error_samples(&self) -> usize {
        self.total_samples().saturating_sub(self.ready_samples())
    }

    pub fn first_ready_at(&self) -> Option<Duration> {
        self.samples
            .iter()
            .find(|sample| sample.is_ready())
            .map(|sample| sample.elapsed)
    }

    pub fn first_error_at(&self) -> Option<Duration> {
        self.samples
            .iter()
            .find(|sample| !sample.is_ready())
            .map(|sample| sample.elapsed)
    }

    pub fn longest_error_streak(&self) -> Option<HttpRpcOutage> {
        let mut current_start = None;
        let mut current_end = None;
        let mut longest = None;

        for sample in &self.samples {
            if sample.is_ready() {
                if let (Some(start), Some(end)) = (current_start.take(), current_end.take()) {
                    let outage = HttpRpcOutage::new(start, end);
                    if longest
                        .as_ref()
                        .map(|known: &HttpRpcOutage| outage.duration > known.duration)
                        .unwrap_or(true)
                    {
                        longest = Some(outage);
                    }
                }
            } else {
                current_start.get_or_insert(sample.elapsed);
                current_end = Some(sample.elapsed);
            }
        }

        if let (Some(start), Some(end)) = (current_start, current_end) {
            let outage = HttpRpcOutage::new(start, end);
            if longest
                .as_ref()
                .map(|known: &HttpRpcOutage| outage.duration > known.duration)
                .unwrap_or(true)
            {
                longest = Some(outage);
            }
        }

        longest
    }

    pub fn latest_block_number(&self) -> Option<u64> {
        self.samples
            .iter()
            .rev()
            .find_map(|sample| sample.block_number)
    }

    pub fn head_timeline(&self) -> Vec<HttpRpcHeadObservation> {
        let mut timeline = Vec::new();
        let mut previous: Option<(u64, &str)> = None;

        for sample in &self.samples {
            let Some(block_number) = sample.block_number else {
                continue;
            };
            let Some(block_hash) = sample.latest_block_hash.as_deref() else {
                continue;
            };
            let current = (block_number, block_hash);
            if previous != Some(current) {
                timeline.push(HttpRpcHeadObservation {
                    observed_at: sample.elapsed,
                    block_number,
                    block_hash: block_hash.to_owned(),
                });
                previous = Some(current);
            }
        }

        timeline
    }

    pub fn detailed_timeline(&self) -> Vec<HttpRpcTimelineEntry> {
        let mut entries = Vec::new();
        let mut current_span: Option<HttpRpcStableSpanBuilder> = None;
        let mut previous_state: Option<HttpRpcState> = None;

        for sample in &self.samples {
            let state = HttpRpcState::from_sample(sample);
            if previous_state.as_ref().map(HttpRpcState::key) != Some(state.key()) {
                if let Some(span) = current_span.take().filter(|span| span.has_extra_samples()) {
                    entries.push(HttpRpcTimelineEntry::Stable(span.finish()));
                }
                entries.push(HttpRpcTimelineEntry::Transition(HttpRpcTransition {
                    observed_at: sample.elapsed,
                    description: HttpRpcTransition::describe(previous_state.as_ref(), &state),
                }));
                current_span = Some(HttpRpcStableSpanBuilder::new(sample.elapsed, state.clone()));
                previous_state = Some(state);
            } else if let Some(span) = current_span.as_mut() {
                span.push(sample);
            }
        }

        if let Some(span) = current_span.filter(|span| span.has_extra_samples()) {
            entries.push(HttpRpcTimelineEntry::Stable(span.finish()));
        }

        entries
    }

    pub fn format_detailed_timeline(&self) -> String {
        let entries = self.detailed_timeline();
        if entries.is_empty() {
            return "timeline=none".to_owned();
        }

        let mut previous_transition_at = None;
        entries
            .into_iter()
            .map(|entry| match entry {
                HttpRpcTimelineEntry::Transition(transition) => {
                    let formatted = format_transition(&transition, previous_transition_at);
                    previous_transition_at = Some(transition.observed_at);
                    formatted
                }
                HttpRpcTimelineEntry::Stable(span) => {
                    HttpRpcTimelineEntry::Stable(span).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn assert_eventually_ready(&self) -> anyhow::Result<()> {
        self.first_ready_at().map(|_| ()).with_context(|| {
            format!(
                "{} never became ready over {:?}",
                self.name,
                self.total_duration()
            )
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HttpRpcOutage {
    pub started_at: Duration,
    pub ended_at: Duration,
    pub duration: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpRpcHeadObservation {
    pub observed_at: Duration,
    pub block_number: u64,
    pub block_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpRpcState {
    Available {
        block_number: u64,
        block_hash: String,
    },
    Unavailable {
        error: String,
    },
}

impl HttpRpcState {
    fn from_sample(sample: &HttpRpcSample) -> Self {
        match (
            &sample.status,
            sample.block_number,
            sample.latest_block_hash.as_ref(),
        ) {
            (HttpRpcSampleStatus::Ready, Some(block_number), Some(block_hash)) => Self::Available {
                block_number,
                block_hash: block_hash.clone(),
            },
            _ => Self::Unavailable {
                error: sample.status.short_label(),
            },
        }
    }

    fn key(&self) -> HttpRpcStateKey<'_> {
        match self {
            Self::Available {
                block_number,
                block_hash,
            } => HttpRpcStateKey::Available {
                block_number: *block_number,
                block_hash,
            },
            Self::Unavailable { .. } => HttpRpcStateKey::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpRpcStateKey<'a> {
    Available {
        block_number: u64,
        block_hash: &'a str,
    },
    Unavailable,
}

#[derive(Debug, Clone)]
pub enum HttpRpcTimelineEntry {
    Transition(HttpRpcTransition),
    Stable(HttpRpcStableSpan),
}

#[derive(Debug, Clone)]
pub struct HttpRpcTransition {
    pub observed_at: Duration,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct HttpRpcStableSpan {
    pub started_at: Duration,
    pub ended_at: Duration,
    pub request_count: usize,
    pub avg_latency: Duration,
    pub min_latency: Duration,
    pub max_latency: Duration,
    pub state: HttpRpcState,
    pub error_counts: Vec<(String, usize)>,
}

#[derive(Debug, Clone)]
struct HttpRpcStableSpanBuilder {
    started_at: Duration,
    ended_at: Duration,
    request_count: usize,
    total_latency: Duration,
    min_latency: Duration,
    max_latency: Duration,
    state: HttpRpcState,
    error_counts: BTreeMap<String, usize>,
}

impl HttpRpcStableSpanBuilder {
    fn new(started_at: Duration, state: HttpRpcState) -> Self {
        Self {
            started_at,
            ended_at: started_at,
            request_count: 0,
            total_latency: Duration::ZERO,
            min_latency: Duration::MAX,
            max_latency: Duration::ZERO,
            state,
            error_counts: BTreeMap::new(),
        }
    }

    fn push(&mut self, sample: &HttpRpcSample) {
        self.ended_at = sample.elapsed;
        self.request_count += 1;
        self.total_latency += sample.latency;
        self.min_latency = self.min_latency.min(sample.latency);
        self.max_latency = self.max_latency.max(sample.latency);
        if !sample.is_ready() {
            *self
                .error_counts
                .entry(sample.status.short_label())
                .or_default() += 1;
        }
    }

    fn has_extra_samples(&self) -> bool {
        self.request_count > 0
    }

    fn finish(self) -> HttpRpcStableSpan {
        let avg_latency = if self.request_count == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.total_latency.as_secs_f64() / self.request_count as f64)
        };
        HttpRpcStableSpan {
            started_at: self.started_at,
            ended_at: self.ended_at,
            request_count: self.request_count,
            avg_latency,
            min_latency: if self.request_count == 0 {
                Duration::ZERO
            } else {
                self.min_latency
            },
            max_latency: self.max_latency,
            state: self.state,
            error_counts: self.error_counts.into_iter().collect(),
        }
    }
}

impl fmt::Display for HttpRpcTimelineEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transition(transition) => write!(
                f,
                "{:>8} | {}",
                format_elapsed(transition.observed_at),
                transition.description
            ),
            Self::Stable(span) => write!(f, "         | {}", span.describe()),
        }
    }
}

impl HttpRpcTransition {
    fn describe(previous: Option<&HttpRpcState>, current: &HttpRpcState) -> String {
        match (previous, current) {
            (
                None,
                HttpRpcState::Available {
                    block_number,
                    block_hash,
                },
            ) => format!(
                "rpc became available; latest block={block_number} hash={}",
                short_hash(block_hash)
            ),
            (None, HttpRpcState::Unavailable { error }) => {
                format!("rpc started unavailable; {error}")
            }
            (
                Some(HttpRpcState::Unavailable { .. }),
                HttpRpcState::Available {
                    block_number,
                    block_hash,
                },
            ) => format!(
                "rpc recovered; latest block={block_number} hash={}",
                short_hash(block_hash)
            ),
            (
                Some(HttpRpcState::Available {
                    block_number,
                    block_hash,
                }),
                HttpRpcState::Unavailable { error },
            ) => format!(
                "rpc started failing after latest block={block_number} hash={}; {error}",
                short_hash(block_hash)
            ),
            (
                Some(HttpRpcState::Available {
                    block_number: old_block_number,
                    block_hash: old_block_hash,
                }),
                HttpRpcState::Available {
                    block_number,
                    block_hash,
                },
            ) if block_number > old_block_number => format!(
                "latest block advanced {old_block_number}/{} -> {block_number}/{}",
                short_hash(old_block_hash),
                short_hash(block_hash)
            ),
            (
                Some(HttpRpcState::Available {
                    block_number: old_block_number,
                    block_hash: old_block_hash,
                }),
                HttpRpcState::Available {
                    block_number,
                    block_hash,
                },
            ) if block_number < old_block_number => format!(
                "latest block regressed {old_block_number}/{} -> {block_number}/{}",
                short_hash(old_block_hash),
                short_hash(block_hash)
            ),
            (
                Some(HttpRpcState::Available {
                    block_number,
                    block_hash: old_block_hash,
                }),
                HttpRpcState::Available { block_hash, .. },
            ) => format!(
                "latest block hash changed at block {block_number}: {} -> {}",
                short_hash(old_block_hash),
                short_hash(block_hash)
            ),
            (_, HttpRpcState::Unavailable { error }) => {
                format!("rpc unavailable; {error}")
            }
        }
    }
}

impl HttpRpcStableSpan {
    fn describe(&self) -> String {
        let window_ms = self.ended_at.saturating_sub(self.started_at).as_millis();
        let latency_summary = format!(
            "{} requests over {window_ms}ms; avg latency={}ms min={}ms max={}ms",
            self.request_count,
            self.avg_latency.as_millis(),
            self.min_latency.as_millis(),
            self.max_latency.as_millis()
        );

        match &self.state {
            HttpRpcState::Available {
                block_number,
                block_hash,
            } => format!(
                "{latency_summary}; all successful with unchanged latest block={block_number} hash={}",
                short_hash(block_hash)
            ),
            HttpRpcState::Unavailable { .. } => {
                let error_summary = match self.error_counts.as_slice() {
                    [] => "all failing requests".to_owned(),
                    [(error, count)] => format!("{count}x same failure: {error}"),
                    counts => format!(
                        "failing requests with {} error variants: {}",
                        counts.len(),
                        counts
                            .iter()
                            .map(|(error, count)| format!("{count}x {error}"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                };
                format!("{latency_summary}; {error_summary}")
            }
        }
    }
}

impl HttpRpcOutage {
    fn new(started_at: Duration, ended_at: Duration) -> Self {
        Self {
            started_at,
            ended_at,
            duration: ended_at.saturating_sub(started_at),
        }
    }
}

impl fmt::Display for HttpRpcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let longest_outage = self
            .longest_error_streak()
            .map(|outage| {
                format!(
                    "longest_outage={}ms(start={}ms,end={}ms)",
                    outage.duration.as_millis(),
                    outage.started_at.as_millis(),
                    outage.ended_at.as_millis()
                )
            })
            .unwrap_or_else(|| "longest_outage=none".to_owned());
        write!(
            f,
            "HttpRpcReport{{name={}, url={}, duration={}ms, samples={}, ready_samples={}, error_samples={}, first_ready={}ms, first_error={}, latest_block={:?}, {}}}",
            self.name,
            self.url,
            self.total_duration().as_millis(),
            self.total_samples(),
            self.ready_samples(),
            self.error_samples(),
            self.first_ready_at()
                .map(|it| it.as_millis())
                .unwrap_or_default(),
            self.first_error_at()
                .map(|it| format!("{}ms", it.as_millis()))
                .unwrap_or_else(|| "none".to_owned()),
            self.latest_block_number(),
            longest_outage
        )
    }
}

impl HttpRpcSampleStatus {
    fn short_label(&self) -> String {
        match self {
            Self::Ready => "ready".to_owned(),
            Self::TransportError { message } => format!("transport error: {message}"),
            Self::HttpError { status, body } => format!("http {status}: {body}"),
            Self::JsonRpcError { message } => format!("json-rpc error: {message}"),
            Self::InvalidResponse { message } => format!("invalid response: {message}"),
        }
    }
}

fn short_hash(hash: &str) -> &str {
    let len = if hash.starts_with("0x") { 7 } else { 5 };
    &hash[..hash.len().min(len)]
}

fn format_elapsed(duration: Duration) -> String {
    format!("{}.{:03}s", duration.as_secs(), duration.subsec_millis())
}

fn format_transition(
    transition: &HttpRpcTransition,
    previous_transition_at: Option<Duration>,
) -> String {
    let elapsed = format_elapsed(transition.observed_at);
    let header = previous_transition_at
        .map(|previous| {
            format!(
                "{} (after {})",
                elapsed,
                format_elapsed(transition.observed_at.saturating_sub(previous))
            )
        })
        .unwrap_or(elapsed);
    format!("{:>8} | {}{}", header, transition.description, "")
}

// Uses raw reqwest rather than an alloy provider so we can distinguish error levels:
// transport failure (connection refused) → HTTP error (non-2xx) → JSON-RPC error.
// A higher-level client would collapse these into a single error type.
async fn sample_http_rpc(
    client: &reqwest::Client,
    started_at: Instant,
    url: &str,
) -> HttpRpcSample {
    let elapsed = Instant::now().duration_since(started_at);
    let request_started_at = Instant::now();
    let response = client
        .post(url)
        .json(&json!([
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_chainId",
                "params": [],
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "eth_blockNumber",
                "params": [],
            },
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "eth_getBlockByNumber",
                "params": ["latest", false],
            }
        ]))
        .send()
        .await;
    let latency = request_started_at.elapsed();

    match response {
        Ok(response) => {
            let status = response.status();
            let body = match response.text().await {
                Ok(body) => body,
                Err(err) => {
                    return HttpRpcSample {
                        elapsed,
                        latency,
                        chain_id: None,
                        block_number: None,
                        latest_block_hash: None,
                        status: HttpRpcSampleStatus::InvalidResponse {
                            message: format!("failed reading HTTP response body: {err}"),
                        },
                    };
                }
            };
            if !status.is_success() {
                return HttpRpcSample {
                    elapsed,
                    latency,
                    chain_id: None,
                    block_number: None,
                    latest_block_hash: None,
                    status: HttpRpcSampleStatus::HttpError {
                        status: status.as_u16(),
                        body,
                    },
                };
            }

            match decode_rpc_batch_response(&body) {
                Ok((chain_id, block_number, latest_block_hash)) => HttpRpcSample {
                    elapsed,
                    latency,
                    chain_id: Some(chain_id),
                    block_number: Some(block_number),
                    latest_block_hash: Some(latest_block_hash),
                    status: HttpRpcSampleStatus::Ready,
                },
                Err(message) => HttpRpcSample {
                    elapsed,
                    latency,
                    chain_id: None,
                    block_number: None,
                    latest_block_hash: None,
                    status: message,
                },
            }
        }
        Err(err) => HttpRpcSample {
            elapsed,
            latency,
            chain_id: None,
            block_number: None,
            latest_block_hash: None,
            status: HttpRpcSampleStatus::TransportError {
                message: err.to_string(),
            },
        },
    }
}

fn decode_rpc_batch_response(body: &str) -> Result<(String, u64, String), HttpRpcSampleStatus> {
    let responses: Vec<Value> =
        serde_json::from_str(body).map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: format!("failed to decode JSON-RPC batch response: {err}"),
        })?;
    if responses.len() != 3 {
        return Err(HttpRpcSampleStatus::InvalidResponse {
            message: format!("expected 3 JSON-RPC responses, got {}", responses.len()),
        });
    }

    let chain_id = decode_rpc_result(&responses, 1)?
        .as_str()
        .context("eth_chainId response was not a string")
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })?
        .to_owned();
    let block_number_hex = decode_rpc_result(&responses, 2)?
        .as_str()
        .context("eth_blockNumber response was not a string")
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })?;
    let block_number = u64::from_str_radix(
        block_number_hex
            .strip_prefix("0x")
            .unwrap_or(block_number_hex),
        16,
    )
    .with_context(|| format!("invalid eth_blockNumber hex value: {block_number_hex}"))
    .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
        message: err.to_string(),
    })?;
    let latest_block = decode_rpc_result(&responses, 3)?
        .as_object()
        .context("eth_getBlockByNumber response was not an object")
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })?;
    let latest_block_hash = latest_block
        .get("hash")
        .and_then(Value::as_str)
        .context("eth_getBlockByNumber response did not contain a string hash")
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })?
        .to_owned();

    Ok((chain_id, block_number, latest_block_hash))
}

fn decode_rpc_result(responses: &[Value], id: u64) -> Result<&Value, HttpRpcSampleStatus> {
    let response = responses
        .iter()
        .find(|response| response.get("id").and_then(Value::as_u64) == Some(id))
        .with_context(|| format!("missing JSON-RPC response with id={id}"))
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })?;

    if let Some(error) = response.get("error") {
        return Err(HttpRpcSampleStatus::JsonRpcError {
            message: error.to_string(),
        });
    }

    response
        .get("result")
        .with_context(|| format!("missing JSON-RPC result for id={id}"))
        .map_err(|err| HttpRpcSampleStatus::InvalidResponse {
            message: err.to_string(),
        })
}
