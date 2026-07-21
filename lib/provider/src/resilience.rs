//! The L1-outage policy for long-running components.
//!
//! An unreachable L1 (provider outage, network partition, DNS failure) is a routine
//! infrastructure condition: every background component that talks to L1 must wait it
//! out — warn, back off, retry — never die. A node death here multiplies: a shared
//! RPC provider outage would kill every validator of a committee at once.
//!
//! The boundary is *transport vs. semantics*: errors where the server never answered
//! (connection refused, timeouts, HTTP 5xx, exhausted transport retries) are waited
//! out; errors the server actually returned (JSON-RPC error responses) and all
//! post-fetch validation stay fail-fast. Startup first-contact also stays fail-fast —
//! a node that cannot reach L1 even once is misconfigured, and the operator should
//! hear about it immediately.

use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use std::time::Duration;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether `err`'s chain contains a transport-level L1 failure — the "L1 is
/// unreachable" class. JSON-RPC *error responses* are not transient: the server
/// answered, so the problem is semantic and must keep failing fast.
/// Whether one link of an error chain is a transport-level failure.
///
/// Three shapes cover the field: a [`TransportError`] directly (only its
/// `Transport` variant counts — the others mean the server answered), an
/// [`alloy::contract::Error`] wrapping one, and a bare [`TransportErrorKind`]
/// leaf. Matching must happen at these typed levels: everything below them is
/// type-erased (`#[error(transparent)]` wrappers skip chain links, and the
/// retry layer's "max retries exceeded" is a string-based custom error).
fn cause_is_transport(cause: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(transport) = cause.downcast_ref::<TransportError>() {
        return matches!(transport, RpcError::Transport(_));
    }
    if let Some(contract) = cause.downcast_ref::<alloy::contract::Error>() {
        return matches!(
            contract,
            alloy::contract::Error::TransportError(RpcError::Transport(_))
        );
    }
    cause.downcast_ref::<TransportErrorKind>().is_some()
}

pub fn is_transient_l1_error(err: &anyhow::Error) -> bool {
    err.chain().any(cause_is_transport)
}

/// [`is_transient_l1_error`] for plain error types outside an `anyhow` chain.
pub fn error_chain_is_transient(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(current) = cause {
        if cause_is_transport(current) {
            return true;
        }
        cause = current.source();
    }
    false
}

/// Runs `op` until it succeeds or fails for a non-transient reason. Transient L1
/// failures are waited out with a warn and capped exponential backoff — this is how
/// a background component expresses "L1 is down, I will resume when it returns".
pub async fn until_l1_available<T, F, Fut>(component: &'static str, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if is_transient_l1_error(&err) => {
                tracing::warn!(
                    component,
                    retry_in = ?backoff,
                    "L1 unreachable ({err:#}); waiting for it to come back"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::TransportErrorKind;

    #[test]
    fn transport_errors_are_transient_through_anyhow_chains() {
        let raw: TransportError = TransportErrorKind::custom_str("connection refused");
        let wrapped = anyhow::Error::from(raw).context("failed to fetch protocol version");
        assert!(is_transient_l1_error(&wrapped));
    }

    #[test]
    fn rpc_error_responses_are_not_transient() {
        let raw: TransportError = RpcError::ErrorResp(alloy::rpc::json_rpc::ErrorPayload {
            code: -32602,
            message: "BlockOutOfRangeError".into(),
            data: None,
        });
        let wrapped = anyhow::Error::from(raw).context("call failed");
        assert!(!is_transient_l1_error(&wrapped));
    }

    #[tokio::test]
    async fn retries_transient_failures_until_success() {
        let mut attempts = 0;
        let result: anyhow::Result<u32> = until_l1_available("test", || {
            attempts += 1;
            let fail = attempts < 3;
            async move {
                if fail {
                    Err(anyhow::Error::from(TransportErrorKind::custom_str(
                        "refused",
                    )))
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn non_transient_errors_propagate_immediately() {
        let mut attempts = 0;
        let result: anyhow::Result<u32> = until_l1_available("test", || {
            attempts += 1;
            async { Err(anyhow::anyhow!("semantic violation")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }
}
