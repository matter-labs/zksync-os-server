use alloy::transports::{RpcError, TransportErrorKind};

/// Returns `true` for transient infrastructure failures that are worth retrying
/// with backoff (network timeouts, provider restarts, rate limits).
///
/// Mempool rejections and other definitive L1-level errors return `false` and
/// should propagate as fatal errors.
pub(crate) fn is_transient(err: &anyhow::Error) -> bool {
    if let Some(rpc) = err.downcast_ref::<RpcError<TransportErrorKind>>() {
        // `is_retryable()` is only available on `RpcError<TransportErrorKind, _>` with a
        // concrete second type parameter; the monomorphised `RpcError<TransportErrorKind>`
        // alias does not expose it in the version of alloy currently in use.
        // `RpcError::Transport(_)` matches exactly the cases `is_retryable()` would cover
        // for this error kind, so the behaviour is equivalent.
        return matches!(rpc, RpcError::Transport(_));
    }
    false
}

/// Returns `true` when the provider rejected the transaction because our nonce
/// is already used — a prior tx was mined between our nonce fetch and our send.
///
/// Detection is message-based because the EVM error code (-32000) is shared
/// across many error classes.
pub(crate) fn is_nonce_too_low(err: &anyhow::Error) -> bool {
    if let Some(rpc) = err.downcast_ref::<RpcError<TransportErrorKind>>() {
        if let RpcError::ErrorResp(payload) = rpc {
            // TODO: extend with alternative provider phrasings ("replacement transaction underpriced",
            //       "already known") if non-standard L1 providers are added.
            return payload.message.to_ascii_lowercase().contains("nonce too low");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::{RpcError, TransportErrorKind};
    use serde_json::value::RawValue;

    // Wraps RpcError<TransportErrorKind, Box<RawValue>> (= TransportError) in anyhow,
    // matching the type that alloy provider methods propagate through `?`.
    fn transport_err() -> anyhow::Error {
        anyhow::Error::new(TransportErrorKind::backend_gone())
    }

    // Build an anyhow::Error wrapping an RPC error-response with the given message.
    // We deserialize from JSON rather than constructing ErrorPayload directly, since
    // ErrorPayload is defined in alloy_json_rpc which is only a transitive dependency.
    fn rpc_error_resp(message: &str) -> anyhow::Error {
        let json = format!(r#"{{"code":-32000,"message":"{}"}}"#, message);
        let payload: RpcError<TransportErrorKind, Box<RawValue>> =
            RpcError::ErrorResp(serde_json::from_str(&json).expect("valid error payload json"));
        anyhow::Error::new(payload)
    }

    #[test]
    fn transport_error_is_transient() {
        assert!(is_transient(&transport_err()));
    }

    #[test]
    fn rpc_error_resp_is_not_transient() {
        assert!(!is_transient(&rpc_error_resp("some mempool rejection")));
    }

    #[test]
    fn nonce_too_low_message_detected() {
        assert!(is_nonce_too_low(&rpc_error_resp("nonce too low")));
    }

    #[test]
    fn nonce_too_low_case_insensitive() {
        assert!(is_nonce_too_low(&rpc_error_resp("Nonce Too Low")));
    }

    #[test]
    fn unrelated_rpc_error_is_not_nonce_too_low() {
        assert!(!is_nonce_too_low(&rpc_error_resp("execution reverted")));
    }

    #[test]
    fn transport_error_is_not_nonce_too_low() {
        assert!(!is_nonce_too_low(&transport_err()));
    }
}
