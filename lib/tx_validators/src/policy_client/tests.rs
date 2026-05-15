//! Unit + integration tests for `PolicyClient` / `PolicySession`.
//!
//! Transport exercised: **HTTP** (`http://`) via httpmock covers the full
//! request/response surface (allow, deny, fail-closed, bypass, serialization,
//! timeout, protocol-version, bearer-token injection). Construction-level URL
//! checks are covered separately and don't need a server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, U256, address};
use httpmock::{Method, MockServer};
use serde_json::json;
use tokio::task::spawn_blocking;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    BeginTxContext, CallModifier, EvmRequest, EvmResources, EvmTracer, TxValidator,
};

use super::{AccessType, Component, Config, PolicyClient, PolicySession, Tracer};

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

fn base_config(server: &MockServer) -> Config {
    Config {
        url: server.base_url(),
        component: Component::Rpc,
        request_timeout: Duration::from_millis(500),
        protocol_version: "1".into(),
        expected_protocol_version: None,
        bypass_from: Default::default(),
        auth_token: Some("test-token".into()),
    }
}

/// Drives the blocking validator call from a tokio task. In production this
/// path runs inside `spawn_blocking` (see `VmWrapper::new`) so the test
/// mirrors that exactly — `Handle::block_on` needs a blocking thread.
async fn run_begin_tx(
    mut session: PolicySession,
    ctx: BeginTxContext<'static>,
) -> Result<(), InvalidTransaction> {
    spawn_blocking(move || session.begin_tx(&ctx))
        .await
        .unwrap()
}

// ---------- Admit path ----------

#[tokio::test]
async fn happy_path_allow() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn deny_maps_to_filtered_by_validator() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({
            "allow": false,
            "ruleId": "allowed_method_callers",
            "reason": "signer not in whitelist"
        }));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn non_success_status_fails_closed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(503).body("unavailable");
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn malformed_body_fails_closed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).body("not json at all");
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn timeout_fails_closed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200)
            .json_body(json!({"allow": true}))
            .delay(Duration::from_millis(300));
    });
    let mut cfg = base_config(&server);
    cfg.request_timeout = Duration::from_millis(50);
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn connection_refused_fails_closed() {
    // Bind and immediately release a port to get a guaranteed-free port number.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let client = PolicyClient::new(Config {
        url: format!("http://127.0.0.1:{port}"),
        component: Component::Rpc,
        auth_token: Some("test-token".into()),
        request_timeout: Duration::from_millis(500),
        protocol_version: "1".into(),
        expected_protocol_version: None,
        bypass_from: Default::default(),
    })
    .unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_mismatch_fails_closed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200)
            .json_body(json!({"allow": true, "protocolVersion": "2"}));
    });
    let mut cfg = base_config(&server);
    cfg.expected_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_match_allows() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200)
            .json_body(json!({"allow": true, "protocolVersion": "1"}));
    });
    let mut cfg = base_config(&server);
    cfg.expected_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn serialized_request_matches_context() {
    let server = MockServer::start();
    let body: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let body_c = body.clone();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/admit")
            .is_true(move |req| {
                *body_c.lock().unwrap() = Some(req.body().as_ref().to_vec());
                true
            });
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Write), test_context()).await;

    let recorded = body.lock().unwrap().take().expect("body captured");
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
    let server = MockServer::start();
    let body: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let body_c = body.clone();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/admit")
            .is_true(move |req| {
                *body_c.lock().unwrap() = Some(req.body().as_ref().to_vec());
                true
            });
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let _ = run_begin_tx(client.session(AccessType::Read), test_context()).await;

    let recorded = body.lock().unwrap().take().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
    assert_eq!(parsed["accessType"].as_str().unwrap(), "read");
}

#[tokio::test]
async fn bearer_token_sent_when_configured() {
    let server = MockServer::start();
    // Mock only matches if the correct Authorization header is present.
    // A wrong or missing header → 404 → fail-closed → test would panic.
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/admit")
            .header("authorization", "Bearer secret-token");
        then.status(200).json_body(json!({"allow": true}));
    });
    let mut cfg = base_config(&server);
    cfg.auth_token = Some("secret-token".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;
    assert!(
        res.is_ok(),
        "request with correct auth token should succeed"
    );
}

#[test]
fn unsupported_scheme_rejected_at_construction() {
    assert!(
        PolicyClient::new(Config {
            url: "ftp://example.com".into(),
            component: Component::Rpc,
            auth_token: None,
            request_timeout: Duration::from_millis(500),
            protocol_version: "1".into(),
            expected_protocol_version: None,
            bypass_from: Default::default(),
        })
        .is_err()
    );
}

#[test]
fn invalid_url_rejected_at_construction() {
    assert!(
        PolicyClient::new(Config {
            url: "not a url".into(),
            component: Component::Rpc,
            auth_token: None,
            request_timeout: Duration::from_millis(500),
            protocol_version: "1".into(),
            expected_protocol_version: None,
            bypass_from: Default::default(),
        })
        .is_err()
    );
}

#[test]
fn http_url_accepted_at_construction() {
    let mut cfg = Config {
        url: "http://policy.local:9000".into(),
        component: Component::Rpc,
        auth_token: Some("token".into()),
        request_timeout: Duration::from_millis(500),
        protocol_version: "1".into(),
        expected_protocol_version: None,
        bypass_from: Default::default(),
    };
    cfg.auth_token = Some("token".into());
    assert!(PolicyClient::new(cfg).is_ok());
}

#[test]
fn https_url_rejected_at_construction() {
    assert!(
        PolicyClient::new(Config {
            url: "https://policy.local:9000".into(),
            component: Component::Rpc,
            auth_token: None,
            request_timeout: Duration::from_millis(500),
            protocol_version: "1".into(),
            expected_protocol_version: None,
            bypass_from: Default::default(),
        })
        .is_err()
    );
}

#[tokio::test]
async fn bypass_from_skips_admit_call() {
    // Mock configured to deny everything. If the bypass is not honoured the
    // tx fails closed. The Ok assertion at the end proves it never reached the mock.
    let server = MockServer::start();
    let admit_mock = server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": false}));
    });
    let mut cfg = base_config(&server);
    cfg.bypass_from = [FROM].into_iter().collect();
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client.session(AccessType::Write), test_context()).await;

    assert!(res.is_ok(), "bypassed tx should be allowed without a call");
    assert_eq!(
        admit_mock.calls(),
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
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let judge_mock = server.mock(|when, then| {
        when.method(Method::POST).path("/judge");
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "expected judge to allow, got {res:?}");
    assert_eq!(judge_mock.calls(), 1);
}

#[tokio::test]
async fn judge_deny_maps_to_filtered_by_validator() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    server.mock(|when, then| {
        when.method(Method::POST).path("/judge");
        then.status(200).json_body(json!({
            "allow": false,
            "ruleId": "post_exec_disallowed",
            "reason": "wrote to a forbidden slot"
        }));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_transport_error_fails_closed() {
    // No /judge mock registered: the mock server replies 404 to that path,
    // which the client must treat as fail-closed.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_bypass_from_skips_call() {
    // Mock /judge to deny — the bypass must prevent the call from ever firing.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let judge_mock = server.mock(|when, then| {
        when.method(Method::POST).path("/judge");
        then.status(200).json_body(json!({"allow": false}));
    });
    let mut cfg = base_config(&server);
    cfg.bypass_from = [FROM].into_iter().collect();
    let client = PolicyClient::new(cfg).unwrap();
    let session = client.session(AccessType::Write);
    let tracer = session.paired_tracer();
    let res = run_full_tx(session, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "bypassed tx should not be judged");
    assert_eq!(
        judge_mock.calls(),
        0,
        "bypass must not reach the policy service"
    );
}

#[tokio::test]
async fn judge_serialized_request_carries_captured_frames() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let body: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let body_c = body.clone();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/judge")
            .is_true(move |req| {
                *body_c.lock().unwrap() = Some(req.body().as_ref().to_vec());
                true
            });
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
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

    let captured = body.lock().unwrap().take().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&captured).unwrap();
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
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let body: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let body_c = body.clone();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/judge")
            .is_true(move |req| {
                *body_c.lock().unwrap() = Some(req.body().as_ref().to_vec());
                true
            });
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
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

    let captured = body.lock().unwrap().take().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&captured).unwrap();
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
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let body: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let body_c = body.clone();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/judge")
            .is_true(move |req| {
                *body_c.lock().unwrap() = Some(req.body().as_ref().to_vec());
                true
            });
        then.status(200).json_body(json!({"allow": true}));
    });
    let client = PolicyClient::new(base_config(&server)).unwrap();
    let session = client.session(AccessType::Read);
    let tracer = session.paired_tracer();
    let _ = run_full_tx(session, tracer, test_context(), one_frame()).await;

    let captured = body.lock().unwrap().take().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&captured).unwrap();
    assert_eq!(parsed["accessType"].as_str().unwrap(), "read");
}

// ---------- session() isolation ----------

/// Two sessions created from the same `PolicyClient` must each own their
/// own trace slot and `pending_tx_from`. After driving a frame into one
/// session, the other's `/judge` body shows zero frames.
///
/// Uses content-based mock conditions (pure predicates) rather than
/// side-effect body capture to avoid double-counting httpmock's two-phase
/// condition evaluation.
#[tokio::test]
async fn session_isolates_slot_from_sibling() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    // session_a: read intent + a non-null frame with calldata=0xcc.
    let session_a_judge = server.mock(|when, then| {
        when.method(Method::POST).path("/judge").is_true(|req| {
            let v: serde_json::Value =
                serde_json::from_slice(req.body().as_ref()).unwrap_or(json!(null));
            v["accessType"].as_str() == Some("read")
                && !v["trace"]["frame"].is_null()
                && v["trace"]["frame"]["calldata"].as_str() == Some("0xcc")
        });
        then.status(200).json_body(json!({"allow": true}));
    });
    // session_b: write intent + null frame (no tracer frames driven).
    let session_b_judge = server.mock(|when, then| {
        when.method(Method::POST).path("/judge").is_true(|req| {
            let v: serde_json::Value =
                serde_json::from_slice(req.body().as_ref()).unwrap_or(json!(null));
            v["accessType"].as_str() == Some("write") && v["trace"]["frame"].is_null()
        });
        then.status(200).json_body(json!({"allow": true}));
    });

    let client = PolicyClient::new(base_config(&server)).unwrap();
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

    spawn_blocking(move || session_a.finish_tx())
        .await
        .unwrap()
        .unwrap();
    spawn_blocking(move || session_b.finish_tx())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        session_a_judge.calls(),
        1,
        "session_a: read intent with non-null frame carrying calldata=0xcc"
    );
    assert_eq!(
        session_b_judge.calls(),
        1,
        "session_b: write intent with null frame — no bleed from session_a"
    );
}

/// Two concurrent sessions must not see each other's frames at `/judge`.
/// Catches a future regression that shares the slot across concurrent RPC
/// simulations.
#[tokio::test]
async fn concurrent_sessions_dont_share_slot() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({"allow": true}));
    });
    let judge_mock = server.mock(|when, then| {
        when.method(Method::POST).path("/judge");
        then.status(200).json_body(json!({"allow": true}));
    });

    let client = PolicyClient::new(base_config(&server)).unwrap();

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
    assert_eq!(judge_mock.calls(), 2);
}
