/// Returns `true` if `err` signals that the nonce we submitted with is already used.
///
/// This can happen when a previous replacement transaction was accepted by the node and
/// mined while we were still polling for the original hash.  In that case, any further
/// attempt to send with the same nonce will be rejected with a "nonce too low" error,
/// and we should treat the nonce slot as confirmed rather than retrying the send.
pub fn is_nonce_too_low(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("nonce too low") || msg.contains("nonce too small")
}

/// Returns `true` if `err` is likely transient and the caller should retry.
///
/// Currently everything that is not a nonce conflict is considered transient,
/// which is conservative but safe — we'd rather retry a recoverable error than
/// give up prematurely on a live transaction.
pub fn is_transient(err: &anyhow::Error) -> bool {
    !is_nonce_too_low(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_too_low_detection() {
        let cases: &[(&str, bool)] = &[
            ("nonce too low", true),
            ("Nonce Too Low: expected 5 got 4", true),
            ("nonce too small", true),
            ("out of gas", false),
            ("network error", false),
            ("replacement transaction underpriced", false),
        ];
        for &(msg, expected) in cases {
            let err = anyhow::anyhow!("{msg}");
            assert_eq!(
                is_nonce_too_low(&err),
                expected,
                "is_nonce_too_low({msg:?}) should be {expected}"
            );
        }
    }

    #[test]
    fn is_transient_is_inverse_of_nonce_too_low() {
        let nonce_err = anyhow::anyhow!("nonce too low");
        let other_err = anyhow::anyhow!("network timeout");
        assert!(!is_transient(&nonce_err));
        assert!(is_transient(&other_err));
    }
}
