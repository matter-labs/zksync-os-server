//! Typed wrappers for the second proof-system (ZiSK) serialized payloads.
//!
//! The server moves two distinct ZiSK payloads through node-internal channels.
//! Each wrapper marks one payload so the two never mix by accident. Both stay
//! off the shared `ProverInput`. Both wrapper types live in lib crates and are
//! re-exported here so the node channel paths name them from one module.
//!
//! The batch-level [`ZiskBatchBytes`] is what the proving lane hands to a
//! prover, so it lives in `zisk_prover_lane`. Per-block bytes never leave the
//! seal path, so they need no node-side alias.

/// Serialized batch-level ZiSK `BatchInput` (bincode). Defined in the ZiSK
/// proving-lane crate, which is what hands it to a prover; re-exported so the
/// node paths stay stable.
pub use zisk_prover_lane::ZiskBatchBytes;

/// Run a ZiSK-side builder, flattening a panic into an error.
///
/// The ZiSK builder runs inline in the batcher's seal path, itself a critical
/// blocking task, so an
/// out-of-spec guest input (malformed upgrade calldata, an unexpected
/// preimage length) must degrade like any other builder error. Without this
/// the panic unwinds out of the critical task and takes the node down, and a
/// deterministic one turns every replay of that block into the same crash.
/// The payload downcast mirrors the guest executor's own guard in
/// `zisk_prover_lane::shadow`.
pub(crate) fn catch_zisk_panic<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            Err(anyhow::anyhow!("ZiSK builder panicked: {msg}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::catch_zisk_panic;

    #[test]
    fn panic_becomes_error() {
        let result: anyhow::Result<()> = catch_zisk_panic(|| panic!("boom"));
        let message = result.unwrap_err().to_string();
        assert!(message.contains("panicked"), "got: {message}");
        assert!(message.contains("boom"), "got: {message}");
    }

    #[test]
    fn error_passes_through() {
        let result: anyhow::Result<()> = catch_zisk_panic(|| anyhow::bail!("bad block"));
        assert!(result.unwrap_err().to_string().contains("bad block"));
    }

    #[test]
    fn ok_passes_through() {
        assert_eq!(catch_zisk_panic(|| Ok(7u32)).unwrap(), 7);
    }
}
