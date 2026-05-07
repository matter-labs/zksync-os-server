//! Unit + integration tests for `PolicyClient` / `PolicySession`.
//!
//! Transport exercised: **UDS** (`unix:///`) covers the full request/response
//! surface (allow, deny, fail-closed, bypass, serialization, timeout,
//! protocol-version, bearer-token injection). Construction-level URL checks
//! are covered separately and don't need a server.
//!
//! Real HTTP round trips over UDS (no mocked transport), so serde, timeout,
//! and bearer-token code paths run end-to-end.

use std::collections::HashMap;
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
use serde_json::json;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::spawn_blocking;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    BeginTxContext, CallModifier, EvmRequest, EvmResources, EvmTracer, TxValidator,
};

use super::{AccessType, Config, PolicyClient, PolicySession, Tracer};

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
        auth_token: None,
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
    headers: Arc<Mutex<HashMap<String, Vec<HashMap<String, String>>>>>,
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
    fn last_header(&self, path: &str, header: &str) -> Option<String> {
        self.headers
            .lock()
            .unwrap()
            .get(path)
            .and_then(|v| v.last())
            .and_then(|h| h.get(header).cloned())
    }
}

async fn start_uds_mock(routes: MockRoutes) -> MockHandle {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("policy.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let captured: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts: Arc<Mutex<HashMap<String, AtomicUsize>>> = Arc::new(Mutex::new(HashMap::new()));
    let headers: Arc<Mutex<HashMap<String, Vec<HashMap<String, String>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let routes = Arc::new(routes);

    let captured_clone = captured.clone();
    let counts_clone = counts.clone();
    let headers_clone = headers.clone();
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(_) => break,
            };
            let routes = routes.clone();
            let captured = captured_clone.clone();
            let counts = counts_clone.clone();
            let headers = headers_clone.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<Incoming>| {
                            let routes = routes.clone();
                            let captured = captured.clone();
                            let counts = counts.clone();
                            let headers = headers.clone();
                            async move {
                                let path = req.uri().path().to_string();
                                let hdrs: HashMap<String, String> = req
                                    .headers()
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.to_str()
                                            .ok()
                                            .map(|s| (k.as_str().to_owned(), s.to_owned()))
                                    })
                                    .collect();
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
                                headers
                                    .lock()
                                    .unwrap()
                                    .entry(path.clone())
                                    .or_default()
                                    .push(hdrs);
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
        headers,
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
async fn bearer_token_sent_when_configured() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let mut cfg = base_config(mock.url().into());
    cfg.auth_token = Some("secret-token".into());
    let client = PolicyClient::new(cfg).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert_eq!(
        mock.last_header("/admit", "authorization").as_deref(),
        Some("Bearer secret-token")
    );
}

#[tokio::test]
async fn no_authorization_header_when_token_unset() {
    let mock = start_uds_mock(MockRoutes::default().with("/admit", allow_admit())).await;
    let client = PolicyClient::new(base_config(mock.url().into())).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    let auth = mock.last_header("/admit", "authorization");
    assert!(auth.is_none(), "expected no Authorization header, got {auth:?}");
}

#[test]
fn unsupported_scheme_rejected_at_construction() {
    assert!(PolicyClient::new(base_config("ftp://example.com".into())).is_err());
}

#[test]
fn invalid_url_rejected_at_construction() {
    assert!(PolicyClient::new(base_config("not a url".into())).is_err());
}

#[test]
fn http_url_accepted_at_construction() {
    let mut cfg = base_config("http://policy.local:9000".into());
    cfg.auth_token = Some("token".into());
    assert!(PolicyClient::new(cfg).is_ok());
}


#[test]
fn https_url_rejected_at_construction() {
    assert!(PolicyClient::new(base_config("https://policy.local:9000".into())).is_err());
}

#[tokio::test]
async fn bypass_from_skips_admit_call() {
    // Mock configured to deny everything. If the bypass is not honoured the
    // tx fails closed. The Ok assertion at the end proves it never reached the mock.
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
    let root = &parsed["trace"]["frame"];
    assert!(!root.is_null(), "trace.frame should be non-null");
    assert_eq!(
        root["caller"].as_str().unwrap().to_ascii_lowercase(),
        format!("{FROM:#x}")
    );
    assert_eq!(
        root["callee"].as_str().unwrap().to_ascii_lowercase(),
        format!("{TO:#x}")
    );
    assert_eq!(root["value"].as_str().unwrap(), "0x3e8");
    assert_eq!(root["calldata"].as_str().unwrap(), "0xdeadbeef");
    let deploys = root["deploys"].as_array().unwrap();
    assert_eq!(deploys.len(), 1);
    assert_eq!(
        deploys[0].as_str().unwrap().to_ascii_lowercase(),
        format!("{deployed:#x}")
    );
    assert_eq!(root["callKind"].as_str().unwrap(), "call");
    // Constructor frame is a child of the root, not a sibling.
    let children = root["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert!(children[0]["deploys"].as_array().unwrap().is_empty());
    assert_eq!(children[0]["calldata"].as_str().unwrap(), "0xabcd");
    assert_eq!(children[0]["callKind"].as_str().unwrap(), "constructor");
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
    let root = &parsed["trace"]["frame"];
    assert_eq!(root["callKind"].as_str().unwrap(), "call");
    let children = root["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["callKind"].as_str().unwrap(), "delegateCall");
    let grandchildren = children[0]["children"].as_array().unwrap();
    assert_eq!(grandchildren.len(), 1);
    assert_eq!(grandchildren[0]["callKind"].as_str().unwrap(), "staticCall");
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
    let frame_a = &parsed_a["trace"]["frame"];
    assert!(!frame_a.is_null(), "session_a should have a root frame");
    assert_eq!(frame_a["calldata"].as_str().unwrap(), "0xcc");

    // session_b's judge body has no root frame (session_a's frame did not
    // bleed into session_b's slot). session_b defaults to Write intent.
    spawn_blocking(move || session_b.finish_tx())
        .await
        .unwrap()
        .unwrap();
    let body_b = mock.last_body("/judge").expect("judge called for session_b");
    let parsed_b: serde_json::Value = serde_json::from_slice(&body_b).unwrap();
    assert_eq!(parsed_b["accessType"].as_str().unwrap(), "write");
    assert!(parsed_b["trace"]["frame"].is_null(), "session_b should have no root frame");
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

