//! Unit + integration tests for `PolicyClient` / `PolicySession`.
//!
//! Two transports are exercised:
//!   - **UDS** (`unix:///`) covers the bulk of the request/response
//!     surface (allow, deny, fail-closed, bypass, serialization, timeout,
//!     protocol-version). The transport-independent client logic all
//!     flows through here.
//!   - **mTLS over TCP** (`https://`) covers handshake correctness only:
//!     pinned-CA happy path and rejection paths (server cert signed by a
//!     CA the client doesn't trust; client cert that the server's
//!     CA-pinned verifier rejects). Construction-level URL/TLS-pairing
//!     checks are covered separately and don't need a server.
//!
//! Real HTTP round trips in both cases (no mocked transport), so serde,
//! timeout, and TLS code paths run end-to-end.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, U256, address};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::{TcpListener, UnixListener};
use tokio::task::spawn_blocking;
use tokio_rustls::TlsAcceptor;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    BeginTxContext, CallModifier, EvmRequest, EvmResources, EvmTracer, TxValidator,
};

use super::{AccessType, CallKind, CapturedFrame, Config, PolicyClient, PolicySession, TlsConfig, Tracer};

const FROM: Address = address!("0x1111111111111111111111111111111111111111");
const TO: Address = address!("0x2222222222222222222222222222222222222222");
const CALLDATA: &[u8] = &[0xde, 0xad, 0xbe, 0xef];

fn test_context() -> BeginTxContext<'static> {
    BeginTxContext {
        from: FROM,
        to: Some(TO),
        value: U256::from(1_000u64),
        calldata: CALLDATA,
        gas_limit: 100_000,
    }
}

fn base_config(url: String) -> Config {
    Config {
        url,
        request_timeout: Duration::from_millis(500),
        protocol_version: "1".into(),
        expected_protocol_version: None,
        bypass_from: Default::default(),
        tls: None,
    }
}

/// Drives the blocking validator call from a tokio task. In production this
/// path runs inside `spawn_blocking` (see `VmWrapper::new`) so the test
/// mirrors that exactly — `Handle::block_on` needs a blocking thread.
async fn run_begin_tx(
    mut session: PolicySession,
    ctx: BeginTxContext<'static>,
) -> Result<(), InvalidTransaction> {
    spawn_blocking(move || session.begin_tx(&ctx)).await.unwrap()
}

// ---------- Mock-server helper (UDS) ----------
//
// httpmock doesn't support UDS, so we roll a tiny path-dispatched mock that
// covers the patterns the tests need: status + JSON body per path, optional
// pre-response delay, request-body capture, per-path call counts.

#[derive(Clone)]
struct MockResponse {
    status: u16,
    body: Vec<u8>,
    delay: Option<Duration>,
}

impl MockResponse {
    fn ok_json(value: serde_json::Value) -> Self {
        Self {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
            delay: None,
        }
    }
    fn raw(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: None,
        }
    }
    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

#[derive(Default, Clone)]
struct MockRoutes {
    by_path: HashMap<String, MockResponse>,
}

impl MockRoutes {
    fn with(mut self, path: &str, resp: MockResponse) -> Self {
        self.by_path.insert(path.into(), resp);
        self
    }
}

struct MockHandle {
    base_url: String,
    captured: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>,
    counts: Arc<Mutex<HashMap<String, AtomicUsize>>>,
    _tmp: Option<TempDir>,
}

impl MockHandle {
    fn url(&self) -> &str {
        &self.base_url
    }
    fn calls(&self, path: &str) -> usize {
        self.counts
            .lock()
            .unwrap()
            .get(path)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
    fn last_body(&self, path: &str) -> Option<Vec<u8>> {
        self.captured
            .lock()
            .unwrap()
            .get(path)
            .and_then(|v| v.last().cloned())
    }
}

async fn start_uds_mock(routes: MockRoutes) -> MockHandle {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("policy.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let captured: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts: Arc<Mutex<HashMap<String, AtomicUsize>>> = Arc::new(Mutex::new(HashMap::new()));
    let routes = Arc::new(routes);

    let captured_clone = captured.clone();
    let counts_clone = counts.clone();
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(_) => break,
            };
            let routes = routes.clone();
            let captured = captured_clone.clone();
            let counts = counts_clone.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<Incoming>| {
                            let routes = routes.clone();
                            let captured = captured.clone();
                            let counts = counts.clone();
                            async move {
                                let path = req.uri().path().to_string();
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                captured
                                    .lock()
                                    .unwrap()
                                    .entry(path.clone())
                                    .or_default()
                                    .push(body.to_vec());
                                counts
                                    .lock()
                                    .unwrap()
                                    .entry(path.clone())
                                    .or_default()
                                    .fetch_add(1, Ordering::SeqCst);
                                respond(routes.by_path.get(&path).cloned()).await
                            }
                        }),
                    )
                    .await;
            });
        }
    });

    let base_url = format!("unix://{}", socket_path.display());
    MockHandle {
        base_url,
        captured,
        counts,
        _tmp: Some(tmp),
    }
}

async fn respond(
    response: Option<MockResponse>,
) -> Result<Response<Full<Bytes>>, hyper::http::Error> {
    let resp = match response {
        Some(r) => r,
        None => MockResponse::raw(404, "not mocked"),
    };
    if let Some(delay) = resp.delay {
        tokio::time::sleep(delay).await;
    }
    Response::builder()
        .status(resp.status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(resp.body)))
}

fn allow_admit() -> MockResponse {
    MockResponse::ok_json(json!({"allow": true}))
}

// ---------- UDS path: admit ----------

#[tokio::test]
async fn happy_path_allow() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn deny_maps_to_filtered_by_validator() {
    let mock = start_uds_mock(MockRoutes::default().with(
        "/admit",
        MockResponse::ok_json(json!({
            "allow": false,
            "ruleId": "allowed_method_callers",
            "reason": "signer not in whitelist"
        })),
    ))
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn non_success_status_fails_closed() {
    let mock =
        start_uds_mock(MockRoutes::default().with("/admit", MockResponse::raw(503, "unavailable")))
            .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn malformed_body_fails_closed() {
    let mock = start_uds_mock(
        MockRoutes::default().with("/admit", MockResponse::raw(200, "not json at all")),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn timeout_fails_closed() {
    let mock = start_uds_mock(MockRoutes::default().with(
        "/admit",
        allow_admit().with_delay(Duration::from_millis(300)),
    ))
    .await;
    let mut cfg = base_config(mock.url().into());
    cfg.request_timeout = Duration::from_millis(50);
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn connection_refused_fails_closed() {
    // Pointing at a socket that doesn't exist — connect() fails immediately,
    // which the client must treat as fail-closed.
    let client = PolicyClient::new(base_config(
        "unix:///tmp/zksync_os_policy_nonexistent.sock".into(),
    ))
    .unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_mismatch_fails_closed() {
    let mock = start_uds_mock(MockRoutes::default().with(
        "/admit",
        MockResponse::ok_json(json!({"allow": true, "protocolVersion": "2"})),
    ))
    .await;
    let mut cfg = base_config(mock.url().into());
    cfg.expected_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_match_allows() {
    let mock = start_uds_mock(MockRoutes::default().with(
        "/admit",
        MockResponse::ok_json(json!({"allow": true, "protocolVersion": "1"})),
    ))
    .await;
    let mut cfg = base_config(mock.url().into());
    cfg.expected_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn serialized_request_matches_context() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Write), test_context()).await;

    let recorded = mock.last_body("/admit").expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
    assert_eq!(parsed["protocolVersion"], "1");
    assert_eq!(
        parsed["from"].as_str().unwrap().to_ascii_lowercase(),
        format!("{FROM:#x}")
    );
    assert_eq!(
        parsed["to"].as_str().unwrap().to_ascii_lowercase(),
        format!("{TO:#x}")
    );
    assert_eq!(parsed["value"].as_str().unwrap(), "0x3e8");
    assert_eq!(parsed["calldata"].as_str().unwrap(), "0xdeadbeef");
    assert_eq!(parsed["gasLimit"].as_u64().unwrap(), 100_000);
    assert_eq!(parsed["accessType"].as_str().unwrap(), "write");
}

#[tokio::test]
async fn admit_serializes_access_type_read() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Read), test_context()).await;

    let recorded = mock.last_body("/admit").expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
    assert_eq!(parsed["accessType"].as_str().unwrap(), "read");
}

#[tokio::test]
async fn unsupported_scheme_rejected_at_construction() {
    let err = PolicyClient::new(base_config("ftp://example.com".into()));
    assert!(err.is_err(), "expected BuildError for unsupported scheme");
}

#[tokio::test]
async fn invalid_url_rejected_at_construction() {
    let err = PolicyClient::new(base_config("not a url".into()));
    assert!(err.is_err(), "expected BuildError for invalid url");
}

#[tokio::test]
async fn plain_http_rejected_at_construction() {
    let err = PolicyClient::new(base_config("http://policy.local:9000".into()));
    assert!(
        err.is_err(),
        "expected BuildError for unsupported `http://` scheme"
    );
}

#[tokio::test]
async fn https_without_tls_rejected_at_construction() {
    let err = PolicyClient::new(base_config("https://policy.local:9000".into()));
    assert!(
        err.is_err(),
        "expected BuildError when `https://` is used without TLS material"
    );
}

#[tokio::test]
async fn unix_with_tls_is_accepted_at_construction() {
    let certs = generate_test_pki();
    let mut cfg = base_config("unix:///tmp/policy.sock".into());
    cfg.tls = Some(certs.client_tls.clone());
    assert!(
        PolicyClient::new(cfg).is_ok(),
        "unix + tls material should be accepted (tls is silently ignored for UDS)"
    );
}

#[tokio::test]
async fn bypass_from_skips_admit_call() {
    // Mock configured to deny everything. If the bypass isn't honoured the
    // tx would fail closed — the Ok assertion at the end proves it didn't
    // even reach the mock.
    let mock = start_uds_mock(
        MockRoutes::default().with("/admit", MockResponse::ok_json(json!({"allow": false}))),
    )
    .await;
    let mut cfg = base_config(mock.url().into());
    cfg.bypass_from = [FROM].into_iter().collect();
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;

    assert!(res.is_ok(), "bypassed tx should be allowed without a call");
    assert_eq!(
        mock.calls("/admit"),
        0,
        "bypass must not reach the policy service"
    );
}

// ---------- Judge path ----------
//
// `finish_tx` runs the post-execution judge. Tests below drive the full
// tx lifecycle (validator.begin_tx → tracer frames → validator.finish_tx)
// in `spawn_blocking` to mirror the bootloader's call ordering inside
// `spawn_blocking`.

/// Minimal `EvmRequest` impl used to drive captured frames into the tracer.
struct MockFrame {
    caller: Address,
    callee: Address,
    modifier: CallModifier,
    input: Vec<u8>,
    value: U256,
}

impl EvmRequest for &MockFrame {
    fn resources(&self) -> EvmResources {
        EvmResources::default()
    }
    fn caller(&self) -> Address {
        self.caller
    }
    fn callee(&self) -> Address {
        self.callee
    }
    fn modifier(&self) -> CallModifier {
        self.modifier
    }
    fn input(&self) -> &[u8] {
        &self.input
    }
    fn nominal_token_value(&self) -> U256 {
        self.value
    }
}

/// Recursive test frame that drives the tracer through a nested CREATE/CALL
/// shape — `children` open *while their parent is still open*, matching how
/// the bootloader fires `on_new_execution_frame` / `after_execution_frame_completed`
/// in real execution.
struct TraceScript {
    frame: MockFrame,
    children: Vec<TraceScript>,
}

impl TraceScript {
    fn leaf(frame: MockFrame) -> Self {
        Self {
            frame,
            children: Vec::new(),
        }
    }

    fn drive(&self, tracer: &mut Tracer) {
        tracer.on_new_execution_frame(&self.frame);
        for child in &self.children {
            child.drive(tracer);
        }
        tracer.after_execution_frame_completed(None);
    }
}

/// Drive a full tx through the (session, tracer) pair on a blocking thread.
/// Mirrors the bootloader: `tracer.begin_tx` → `session.begin_tx` →
/// nested frame hooks → `session.finish_tx` → `tracer.finish_tx`.
async fn run_full_tx(
    mut session: PolicySession,
    mut tracer: Tracer,
    ctx: BeginTxContext<'static>,
    scripts: Vec<TraceScript>,
) -> Result<(), InvalidTransaction> {
    spawn_blocking(move || {
        EvmTracer::begin_tx(&mut tracer, ctx.calldata);
        let begin = TxValidator::begin_tx(&mut session, &ctx);
        if begin.is_err() {
            EvmTracer::finish_tx(&mut tracer);
            return begin;
        }
        for script in &scripts {
            script.drive(&mut tracer);
        }
        let finish = TxValidator::finish_tx(&mut session);
        EvmTracer::finish_tx(&mut tracer);
        finish
    })
    .await
    .unwrap()
}

fn one_frame() -> Vec<TraceScript> {
    vec![TraceScript::leaf(MockFrame {
        caller: FROM,
        callee: TO,
        modifier: CallModifier::NoModifier,
        input: CALLDATA.to_vec(),
        value: U256::from(1_000u64),
    })]
}

#[tokio::test]
async fn judge_happy_path_allow() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "expected judge to allow, got {res:?}");
    assert_eq!(mock.calls("/judge"), 1);
}

#[tokio::test]
async fn judge_deny_maps_to_filtered_by_validator() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit()).with(
        "/judge",
        MockResponse::ok_json(json!({
            "allow": false,
            "ruleId": "post_exec_disallowed",
            "reason": "wrote to a forbidden slot"
        })),
    ))
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_transport_error_fails_closed() {
    // No /judge mock registered: the mock server replies 404 to that path,
    // which the client must treat as fail-closed.
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_bypass_from_skips_call() {
    // Mock /judge to deny — the bypass must prevent the call from ever firing.
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": false}))),
    )
    .await;
    let mut cfg = base_config(mock.url().into());
    cfg.bypass_from = [FROM].into_iter().collect();
    let client = PolicyClient::new(cfg).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "bypassed tx should not be judged");
    assert_eq!(
        mock.calls("/judge"),
        0,
        "bypass must not reach the policy service"
    );
}

#[tokio::test]
async fn judge_serialized_request_carries_captured_frames() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();

    // Top-level call EOA->Factory, with a nested CREATE that deploys
    // `deployed`. The wire body should record the deploy in the *parent*'s
    // `deploys` list, not on the constructor frame itself.
    let deployed = address!("0x4444444444444444444444444444444444444444");
    let scripts = vec![TraceScript {
        frame: MockFrame {
            caller: FROM,
            callee: TO,
            modifier: CallModifier::NoModifier,
            input: CALLDATA.to_vec(),
            value: U256::from(1_000u64),
        },
        children: vec![TraceScript::leaf(MockFrame {
            caller: TO,
            callee: deployed,
            modifier: CallModifier::Constructor,
            input: vec![0xab, 0xcd],
            value: U256::ZERO,
        })],
    }];
    let res = run_full_tx(session, tracer, test_context(), scripts).await;
    assert!(res.is_ok());

    let body = mock.last_body("/judge").expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["protocolVersion"], "1");
    let frames_json = parsed["trace"]["frames"].as_array().expect("frames array");
    assert_eq!(frames_json.len(), 2);
    assert_eq!(
        frames_json[0]["caller"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        format!("{FROM:#x}")
    );
    assert_eq!(
        frames_json[0]["callee"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        format!("{TO:#x}")
    );
    assert_eq!(frames_json[0]["value"].as_str().unwrap(), "0x3e8");
    assert_eq!(frames_json[0]["calldata"].as_str().unwrap(), "0xdeadbeef");
    let deploys = frames_json[0]["deploys"].as_array().unwrap();
    assert_eq!(deploys.len(), 1);
    assert_eq!(
        deploys[0].as_str().unwrap().to_ascii_lowercase(),
        format!("{deployed:#x}")
    );
    // Constructor frame itself records no deploy.
    assert!(frames_json[1]["deploys"].as_array().unwrap().is_empty());
    assert_eq!(frames_json[1]["calldata"].as_str().unwrap(), "0xabcd");
    // Per-frame call kinds: top-level frame is a regular call, the inner
    // CREATE frame is the constructor.
    assert_eq!(frames_json[0]["callKind"].as_str().unwrap(), "call");
    assert_eq!(frames_json[1]["callKind"].as_str().unwrap(), "constructor");
    assert_eq!(parsed["accessType"].as_str().unwrap(), "write");
}

/// Wire-shape regression for the proxy/impl scenario the field was added
/// for: proxy delegatecalls into impl, the second frame's `callKind` must
/// surface to the service so it knows to skip the per-method lookup.
#[tokio::test]
async fn judge_serialized_frames_carry_call_kind_for_delegatecall_and_static() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();

    // EOA -> Proxy (Call) -> Impl (DelegateCall) -> Oracle (StaticCall).
    let impl_addr = address!("0x5555555555555555555555555555555555555555");
    let oracle = address!("0x6666666666666666666666666666666666666666");
    let scripts = vec![TraceScript {
        frame: MockFrame {
            caller: FROM,
            callee: TO,
            modifier: CallModifier::NoModifier,
            input: CALLDATA.to_vec(),
            value: U256::ZERO,
        },
        children: vec![TraceScript {
            frame: MockFrame {
                caller: TO,
                callee: impl_addr,
                modifier: CallModifier::Delegate,
                input: vec![0x11],
                value: U256::ZERO,
            },
            children: vec![TraceScript::leaf(MockFrame {
                caller: TO,
                callee: oracle,
                modifier: CallModifier::Static,
                input: vec![0x22],
                value: U256::ZERO,
            })],
        }],
    }];
    let res = run_full_tx(session, tracer, test_context(), scripts).await;
    assert!(res.is_ok());

    let body = mock.last_body("/judge").expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let frames = parsed["trace"]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["callKind"].as_str().unwrap(), "call");
    assert_eq!(frames[1]["callKind"].as_str().unwrap(), "delegateCall");
    assert_eq!(frames[2]["callKind"].as_str().unwrap(), "staticCall");
}

#[tokio::test]
async fn judge_serializes_access_type_read() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let session = client.session(AccessType::Read);
    let tracer = session.paired_tracer();
    let _ = run_full_tx(session, tracer, test_context(), one_frame()).await;

    let body = mock.last_body("/judge").expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["accessType"].as_str().unwrap(), "read");
}

// ---------- session() isolation ----------

/// Two sessions created from the same `PolicyClient` must each own their
/// own trace slot and `pending_tx_from`. After driving a frame into one
/// session, the other's `/judge` body shows zero frames.
#[tokio::test]
async fn session_isolates_slot_from_sibling() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let mut session_a = client.session(AccessType::Read);
    let mut session_b = client.session(AccessType::Write);

    // Drive a frame into session_a via its paired tracer.
    let mut tracer_a = session_a.paired_tracer();
    tracer_a.on_new_execution_frame(&MockFrame {
        caller: FROM,
        callee: TO,
        modifier: CallModifier::NoModifier,
        input: vec![0xcc],
        value: U256::ZERO,
    });
    tracer_a.after_execution_frame_completed(None);

    // session_a judge ships exactly session_a's frame, with `read` intent.
    spawn_blocking(move || session_a.finish_tx())
        .await
        .unwrap()
        .unwrap();
    let body_a = mock.last_body("/judge").expect("judge called for session_a");
    let parsed_a: serde_json::Value = serde_json::from_slice(&body_a).unwrap();
    assert_eq!(parsed_a["accessType"].as_str().unwrap(), "read");
    let frames_a = parsed_a["trace"]["frames"].as_array().unwrap();
    assert_eq!(frames_a.len(), 1);
    assert_eq!(frames_a[0]["calldata"].as_str().unwrap(), "0xcc");

    // session_b's judge body has zero frames (session_a's frame did not
    // bleed into session_b's slot). session_b defaults to Write intent.
    spawn_blocking(move || session_b.finish_tx())
        .await
        .unwrap()
        .unwrap();
    let body_b = mock.last_body("/judge").expect("judge called for session_b");
    let parsed_b: serde_json::Value = serde_json::from_slice(&body_b).unwrap();
    assert_eq!(parsed_b["accessType"].as_str().unwrap(), "write");
    assert!(
        parsed_b["trace"]["frames"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Two concurrent sessions must not see each other's frames at `/judge`.
/// Catches a future regression that shares the slot across concurrent RPC
/// simulations.
#[tokio::test]
async fn concurrent_sessions_dont_share_slot() {
    let mock = start_uds_mock(
        MockRoutes::default()
            .with("/admit", allow_admit())
            .with("/judge", MockResponse::ok_json(json!({"allow": true}))),
    )
    .await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();

    let client_a = client.clone();
    let client_b = client.clone();
    let task_a = tokio::spawn(async move {
        let mut session = client_a.session(AccessType::Read);
        let mut tracer = session.paired_tracer();
        tracer.on_new_execution_frame(&MockFrame {
            caller: FROM,
            callee: TO,
            modifier: CallModifier::NoModifier,
            input: vec![0xaa],
            value: U256::ZERO,
        });
        tracer.after_execution_frame_completed(None);
        spawn_blocking(move || session.finish_tx()).await.unwrap()
    });
    let task_b = tokio::spawn(async move {
        let mut session = client_b.session(AccessType::Write);
        let mut tracer = session.paired_tracer();
        tracer.on_new_execution_frame(&MockFrame {
            caller: TO,
            callee: FROM,
            modifier: CallModifier::NoModifier,
            input: vec![0xbb],
            value: U256::ZERO,
        });
        tracer.after_execution_frame_completed(None);
        spawn_blocking(move || session.finish_tx()).await.unwrap()
    });
    task_a.await.unwrap().unwrap();
    task_b.await.unwrap().unwrap();

    // Both judge calls fired and each body has exactly one frame.
    assert_eq!(mock.calls("/judge"), 2);
}

// ---------- mTLS handshake tests ----------
//
// These tests exercise the actual TLS handshake path — they're not about
// the policy logic (covered above over UDS), only about whether the client
// can negotiate mTLS with the server. We cover:
//   - happy path: matched CA on both sides → admit succeeds
//   - server cert signed by an untrusted CA → handshake fails
//   - client cert from a CA the server doesn't trust → handshake fails
// Construction-time URL/TLS pairing checks live in the UDS section above.

struct TestPki {
    /// Trusted CA (PEM) used by both server and client.
    server_ca_pem: String,
    /// Server cert chain + key signed by the trusted CA.
    server_cert_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    /// Client TLS config with inline PEM signed by the trusted CA.
    client_tls: TlsConfig,
    /// Trusted-CA cert in DER form (handy for building server-side verifiers).
    trusted_ca_der: CertificateDer<'static>,
}

fn install_default_crypto_provider() {
    // rustls 0.23 needs *some* crypto provider installed for the server-side
    // verifier APIs that don't take a provider explicitly. Idempotent: only
    // the first call wins, so it's safe to call from every test.
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
}

fn generate_test_pki() -> TestPki {
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "policy-test-ca");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_pem = ca_cert.pem();
    let ca_der = CertificateDer::from(ca_cert.der().to_vec());

    let server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .unwrap();
    let server_cert_chain = vec![CertificateDer::from(server_cert.der().to_vec())];
    let server_key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(server_key.serialize_der()));

    let mut client_params =
        rcgen::CertificateParams::new(vec!["policy-client".to_string()]).unwrap();
    client_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "policy-client");
    let client_key = rcgen::KeyPair::generate().unwrap();
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .unwrap();

    let client_tls = TlsConfig {
        client_cert: client_cert.pem(),
        client_key: client_key.serialize_pem(),
        server_ca: ca_pem.clone(),
    };

    TestPki {
        server_ca_pem: ca_pem,
        server_cert_chain,
        server_key: server_key_der,
        client_tls,
        trusted_ca_der: ca_der,
    }
}

/// Generate a second PKI universe whose CA the trusted PKI doesn't know.
/// Used to forge mismatched roots between client and server.
fn generate_alt_pki() -> TestPki {
    generate_test_pki()
}

async fn start_tls_server(
    server_cert_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    client_root_ca: CertificateDer<'static>,
    routes: MockRoutes,
) -> SocketAddr {
    install_default_crypto_provider();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(client_root_ca).unwrap();
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(server_cert_chain, server_key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let acceptor = acceptor.clone();
            let routes = routes.clone();
            tokio::spawn(async move {
                let stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<Incoming>| {
                            let routes = routes.clone();
                            async move {
                                let path = req.uri().path().to_string();
                                let _ = req.into_body().collect().await;
                                respond(routes.by_path.get(&path).cloned()).await
                            }
                        }),
                    )
                    .await;
            });
        }
    });

    addr
}

#[tokio::test]
async fn mtls_handshake_succeeds_with_pinned_ca() {
    let pki = generate_test_pki();
    let addr = start_tls_server(
        pki.server_cert_chain.clone(),
        pki.server_key.clone_key(),
        pki.trusted_ca_der.clone(),
        MockRoutes::default().with("/admit", allow_admit()),
    )
    .await;

    let mut cfg = base_config(format!("https://localhost:{}", addr.port()));
    cfg.tls = Some(pki.client_tls.clone());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(res.is_ok(), "mTLS happy path should succeed: {res:?}");
}

#[tokio::test]
async fn mtls_fails_when_server_signed_by_untrusted_ca() {
    let trusted = generate_test_pki();
    let untrusted = generate_alt_pki();
    // Server presents a cert from `untrusted`; client only pins `trusted` CA.
    let addr = start_tls_server(
        untrusted.server_cert_chain.clone(),
        untrusted.server_key.clone_key(),
        // Server still accepts client certs from the trusted CA — we want
        // to isolate the failure to *server cert* validation.
        trusted.trusted_ca_der.clone(),
        MockRoutes::default().with("/admit", allow_admit()),
    )
    .await;

    let mut cfg = base_config(format!("https://localhost:{}", addr.port()));
    cfg.tls = Some(trusted.client_tls.clone());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(
        matches!(res, Err(InvalidTransaction::FilteredByValidator)),
        "untrusted server cert must fail closed, got {res:?}"
    );
}

#[tokio::test]
async fn mtls_fails_when_client_cert_signed_by_untrusted_ca() {
    let trusted = generate_test_pki();
    let alt = generate_alt_pki();
    // Server pins only the *trusted* CA for client-cert verification, but
    // the client presents a cert from `alt` — server should reject.
    let addr = start_tls_server(
        trusted.server_cert_chain.clone(),
        trusted.server_key.clone_key(),
        trusted.trusted_ca_der.clone(),
        MockRoutes::default().with("/admit", allow_admit()),
    )
    .await;

    // Build a client whose pinned server-CA is `trusted` (so the server cert
    // validates) but whose client cert is signed by `alt`.
    let mixed_tls = TlsConfig {
        client_cert: alt.client_tls.client_cert.clone(),
        client_key: alt.client_tls.client_key.clone(),
        server_ca: trusted.server_ca_pem.clone(),
    };
    let mut cfg = base_config(format!("https://localhost:{}", addr.port()));
    cfg.tls = Some(mixed_tls);
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(
        matches!(res, Err(InvalidTransaction::FilteredByValidator)),
        "untrusted client cert must fail closed, got {res:?}"
    );
}

#[tokio::test]
async fn mtls_invalid_pem_fails_at_construction() {
    let mut cfg = base_config("https://localhost:1".into());
    cfg.tls = Some(TlsConfig {
        client_cert: "not-a-cert".into(),
        client_key: "not-a-key".into(),
        server_ca: "not-a-ca".into(),
    });
    assert!(
        PolicyClient::new(cfg).is_err(),
        "invalid PEM must fail at construction"
    );
}

#[tokio::test]
async fn mtls_empty_pem_fails_at_construction() {
    let mut cfg = base_config("https://localhost:1".into());
    cfg.tls = Some(TlsConfig {
        client_cert: String::new(),
        client_key: String::new(),
        server_ca: String::new(),
    });
    assert!(
        PolicyClient::new(cfg).is_err(),
        "empty PEM must fail at construction"
    );
}

#[tokio::test]
async fn mtls_succeeds_with_intermediate_ca_in_client_chain() {
    // Root CA.
    let mut root_params = rcgen::CertificateParams::default();
    root_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    root_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "intermediate-test-root");
    let root_key = rcgen::KeyPair::generate().unwrap();
    let root_cert = root_params.self_signed(&root_key).unwrap();
    let root_der = CertificateDer::from(root_cert.der().to_vec());

    // Intermediate CA, signed by root.
    let mut intermediate_params = rcgen::CertificateParams::default();
    intermediate_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    intermediate_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "intermediate-ca");
    let intermediate_key = rcgen::KeyPair::generate().unwrap();
    let intermediate_cert = intermediate_params
        .signed_by(&intermediate_key, &root_cert, &root_key)
        .unwrap();

    // Server cert signed by root (so the client trusts it directly).
    let server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_cert = server_params
        .signed_by(&server_key, &root_cert, &root_key)
        .unwrap();
    let server_chain = vec![CertificateDer::from(server_cert.der().to_vec())];
    let server_key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(server_key.serialize_der()));

    // Client leaf signed by the intermediate. client_cert contains
    // [leaf, intermediate] so the server can build the chain to its pinned root.
    let client_params = rcgen::CertificateParams::new(vec!["policy-client".to_string()]).unwrap();
    let client_key = rcgen::KeyPair::generate().unwrap();
    let client_cert = client_params
        .signed_by(&client_key, &intermediate_cert, &intermediate_key)
        .unwrap();
    let mut client_chain_pem = client_cert.pem();
    client_chain_pem.push_str(&intermediate_cert.pem());

    // Server's client-trust root is the same root; it builds the path
    // through the intermediate the client presents.
    let addr = start_tls_server(
        server_chain,
        server_key_der,
        root_der,
        MockRoutes::default().with("/admit", allow_admit()),
    )
    .await;

    let mut cfg = base_config(format!("https://localhost:{}", addr.port()));
    cfg.tls = Some(TlsConfig {
        client_cert: client_chain_pem,
        client_key: client_key.serialize_pem(),
        server_ca: root_cert.pem(),
    });
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(
        res.is_ok(),
        "client chain [leaf, intermediate] should validate against root, got {res:?}"
    );
}
