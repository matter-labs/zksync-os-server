//! Unit + integration tests for `PolicyClient`.
//!
//! All tests run the client through an `httpmock::MockServer` (or a
//! single-shot hyper UDS server for the UDS case), i.e. a real HTTP round
//! trip — not a mocked transport. This keeps the tests close to production
//! behaviour and covers the serde + timeout paths in the same harness.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256, address};
use httpmock::{Method, MockServer};
use serde_json::json;
use tokio::task::spawn_blocking;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    BeginTxContext, CallModifier, EvmRequest, EvmResources, EvmTracer, TxValidator,
};

use super::{Config, PolicyClient, Tracer};

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
        auth_token: None,
        protocol_version: "1".into(),
        min_protocol_version: None,
        bypass_from: HashSet::new(),
    }
}

/// Drives the blocking validator call from a tokio task. In production this
/// path runs inside `spawn_blocking` (see `VmWrapper::new`) so the test
/// mirrors that exactly — `Handle::block_on` needs a blocking thread.
async fn run_begin_tx(
    mut client: PolicyClient,
    ctx: BeginTxContext<'static>,
) -> Result<(), InvalidTransaction> {
    spawn_blocking(move || client.begin_tx(&ctx)).await.unwrap()
}

#[tokio::test]
async fn happy_path_allow() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST)
                .path("/admit")
                .header("content-type", "application/json");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn deny_maps_to_filtered_by_validator() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({
                "allow": false,
                "ruleId": "allowed_method_callers",
                "reason": "signer not in whitelist"
            }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn non_success_status_fails_closed() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(503).body("unavailable");
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn malformed_body_fails_closed() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).body("not json at all");
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn timeout_fails_closed() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200)
                .delay(Duration::from_millis(300))
                .json_body(json!({ "allow": true }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.request_timeout = Duration::from_millis(50);
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn connection_refused_fails_closed() {
    // Port 1 is reliably refused on localhost in CI/test envs.
    let client = PolicyClient::new(base_config("http://127.0.0.1:1".into())).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_mismatch_fails_closed() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({
                "allow": true,
                "protocolVersion": "2"
            }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.min_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn protocol_version_match_allows() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({
                "allow": true,
                "protocolVersion": "1"
            }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.min_protocol_version = Some("1".into());
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn serialized_request_matches_context() {
    // Round-trip test — confirm every `BeginTxContext` field lands in the
    // JSON body the service receives.
    let server = MockServer::start_async().await;
    let seen_body = Arc::new(std::sync::Mutex::new(None));
    let seen_body_mock = seen_body.clone();

    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST)
                .path("/admit")
                .is_true(move |req| {
                    *seen_body_mock.lock().unwrap() = Some(req.body().to_vec());
                    true
                });
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let _ = run_begin_tx(client, test_context()).await;

    let recorded = seen_body.lock().unwrap().clone().expect("body captured");
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

/// Drive a full tx through the (validator, tracer) pair on a blocking thread.
/// Mirrors the bootloader: `tracer.begin_tx` → `validator.begin_tx` →
/// nested frame hooks → `validator.finish_tx` → `tracer.finish_tx`.
async fn run_full_tx(
    mut client: PolicyClient,
    mut tracer: Tracer,
    ctx: BeginTxContext<'static>,
    scripts: Vec<TraceScript>,
) -> Result<(), InvalidTransaction> {
    spawn_blocking(move || {
        EvmTracer::begin_tx(&mut tracer, ctx.calldata);
        let begin = TxValidator::begin_tx(&mut client, &ctx);
        if begin.is_err() {
            EvmTracer::finish_tx(&mut tracer);
            return begin;
        }
        for script in &scripts {
            script.drive(&mut tracer);
        }
        let finish = TxValidator::finish_tx(&mut client);
        EvmTracer::finish_tx(&mut tracer);
        finish
    })
    .await
    .unwrap()
}

fn allow_admit_mock(server: &MockServer) -> impl Future<Output = httpmock::Mock<'_>> {
    server.mock_async(|when, then| {
        when.method(Method::POST).path("/admit");
        then.status(200).json_body(json!({ "allow": true }));
    })
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
    let server = MockServer::start_async().await;
    let _admit_mock = allow_admit_mock(&server).await;
    let judge_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST)
                .path("/judge")
                .header("content-type", "application/json");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let tracer = client.paired_tracer();
    let res = run_full_tx(client, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "expected judge to allow, got {res:?}");
    assert_eq!(judge_mock.calls_async().await, 1);
}

#[tokio::test]
async fn judge_deny_maps_to_filtered_by_validator() {
    let server = MockServer::start_async().await;
    let _admit_mock = allow_admit_mock(&server).await;
    let _judge_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({
                "allow": false,
                "ruleId": "post_exec_disallowed",
                "reason": "wrote to a forbidden slot"
            }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let tracer = client.paired_tracer();
    let res = run_full_tx(client, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_transport_error_fails_closed() {
    // No /judge mock registered: the mock server replies 404 to that path,
    // which the client must treat as fail-closed.
    let server = MockServer::start_async().await;
    let _admit_mock = allow_admit_mock(&server).await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let tracer = client.paired_tracer();
    let res = run_full_tx(client, tracer, test_context(), one_frame()).await;

    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn judge_bypass_from_skips_call() {
    // Mock /judge to deny — the bypass must prevent the call from ever firing.
    let server = MockServer::start_async().await;
    let _admit_mock = allow_admit_mock(&server).await;
    let judge_deny = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({ "allow": false }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.bypass_from = HashSet::from([FROM]);
    let client = PolicyClient::new(cfg).unwrap();
    let tracer = client.paired_tracer();
    let res = run_full_tx(client, tracer, test_context(), one_frame()).await;

    assert!(res.is_ok(), "bypassed tx should not be judged");
    assert_eq!(
        judge_deny.calls_async().await,
        0,
        "bypass must not reach the policy service"
    );
}

#[tokio::test]
async fn judge_serialized_request_carries_captured_frames() {
    let server = MockServer::start_async().await;
    let _admit_mock = allow_admit_mock(&server).await;

    let captured = Arc::new(std::sync::Mutex::new(None));
    let captured_clone = captured.clone();
    let _judge_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST)
                .path("/judge")
                .is_true(move |req| {
                    *captured_clone.lock().unwrap() = Some(req.body().to_vec());
                    true
                });
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let tracer = client.paired_tracer();

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
    let res = run_full_tx(client, tracer, test_context(), scripts).await;
    assert!(res.is_ok());

    let body = captured.lock().unwrap().clone().expect("body captured");
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
}

#[tokio::test]
async fn bypass_from_skips_admit_call() {
    // Mock configured to deny everything. If the bypass isn't honoured the
    // tx would fail closed — the Ok assertion at the end proves it didn't
    // even reach the mock.
    let server = MockServer::start_async().await;
    let deny_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": false }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.bypass_from = HashSet::from([FROM]);
    let client = PolicyClient::new(cfg).unwrap();
    let res = run_begin_tx(client, test_context()).await;

    assert!(res.is_ok(), "bypassed tx should be allowed without a call");
    assert_eq!(
        deny_mock.calls_async().await,
        0,
        "bypass must not reach the policy service"
    );
}

// ---------- UDS path ----------
//
// Keeps the transport seam honest: same `PolicyClient` surface, different
// URL scheme, a real HTTP round trip over a Unix socket.

#[tokio::test]
async fn uds_happy_path_allow() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixListener;

    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("policy.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    // One-shot server: accept a connection, respond once to `POST /admit`,
    // then close.
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            let service = service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                assert_eq!(req.method(), hyper::Method::POST);
                assert_eq!(req.uri().path(), "/admit");
                // Drain the body so the framing is clean.
                let _ = req.into_body().collect().await;
                Ok::<_, hyper::Error>(
                    hyper::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from(r#"{"allow": true}"#)))
                        .unwrap(),
                )
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        }
    });

    let url = format!("unix://{}", socket_path.display());
    let client = PolicyClient::new(base_config(url)).unwrap();
    let res = run_begin_tx(client, test_context()).await;
    assert!(res.is_ok(), "expected UDS admit to succeed, got {res:?}");
}

// ---------- Async admit path ----------
//
// `admit` is the RPC-side entry point: it runs the same logic as
// `begin_tx` but stays async so handlers don't need `spawn_blocking`.

#[tokio::test]
async fn async_admit_happy_path_allow() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = client.admit(&test_context()).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn async_admit_deny_maps_to_filtered_by_validator() {
    let server = MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": false }));
        })
        .await;

    let client = PolicyClient::new(base_config(server.base_url())).unwrap();
    let res = client.admit(&test_context()).await;
    assert!(matches!(res, Err(InvalidTransaction::FilteredByValidator)));
}

#[tokio::test]
async fn async_admit_bypass_skips_call() {
    let server = MockServer::start_async().await;
    let deny_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": false }));
        })
        .await;

    let mut cfg = base_config(server.base_url());
    cfg.bypass_from = HashSet::from([FROM]);
    let client = PolicyClient::new(cfg).unwrap();
    let res = client.admit(&test_context()).await;

    assert!(res.is_ok(), "bypassed tx should be allowed without a call");
    assert_eq!(
        deny_mock.calls_async().await,
        0,
        "bypass must not reach the policy service"
    );
}
