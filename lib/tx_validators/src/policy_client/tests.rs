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
use zksync_os_interface::tracing::{BeginTxContext, TxValidator};

use super::{Config, PolicyClient};

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

#[tokio::test]
async fn finish_tx_is_stub_ok() {
    let server = MockServer::start_async().await;
    let mut client = PolicyClient::new(base_config(server.base_url())).unwrap();
    // `finish_tx` is a stub until the execution-result judge lands.
    assert!(client.finish_tx().is_ok());
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
