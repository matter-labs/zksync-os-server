/// Categorized error from the L1 sender's main send loop.
///
/// This lets the send loop distinguish between errors that resolve on their own
/// (transient), errors that require waiting for an external condition to change
/// (recoverable), and errors that require manual intervention (fatal).
///
/// Note: startup errors (operator registration, initial balance check, passthrough
/// handling) run before the main loop and are always Fatal — they are not covered
/// by this type.
#[derive(Debug)]
pub enum L1SendError {
    /// Temporary infrastructure issue: RPC down, timeout, rate limit.
    /// The sender retries with exponential backoff.
    Transient(anyhow::Error),
    /// External condition is blocking progress: gas too high, tx stuck, nonce conflict.
    /// The sender waits for the condition to resolve, then retries.
    Recoverable {
        reason: RecoverableReason,
        source: anyhow::Error,
    },
    /// Unrecoverable error that requires manual intervention.
    /// The sender crashes the binary by propagating this as `anyhow::Error`.
    Fatal(anyhow::Error),
}

/// The specific reason a `Recoverable` error was returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverableReason {
    /// Network gas fees exceed the configured cap. Wait for congestion to pass.
    GasBlocked,
    /// Blob base fee exceeds the configured cap. Wait for blob demand to drop.
    BlobFeeBlocked,
    /// Transaction was not included in L1 within the timeout window.
    ///
    /// Known limitation: the timed-out transaction may still be in the mempool.
    /// The command is re-queued for resubmission. If the original tx later lands
    /// on L1 with a conflicting nonce, the retry will fail with `NonceTooLow`
    /// (which is also recoverable).
    TxTimeout,
    /// Mempool rejected the transaction because the nonce was already used.
    /// Usually happens when a prior tx is mined between fee estimation and submit.
    NonceTooLow,
}

impl L1SendError {
    /// Extracts the inner `anyhow::Error`, regardless of variant.
    ///
    /// Used when returning from `run()` after a Fatal error, or when escalating
    /// a non-Fatal error to the pipeline (which expects `anyhow::Result`).
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Transient(e) | Self::Fatal(e) => e,
            Self::Recoverable { source, .. } => source,
        }
    }

    /// Classifies a `send_raw_transaction` RPC error as Transient or Recoverable.
    ///
    /// Parses the error message for known "nonce too low" patterns emitted by
    /// common Ethereum clients (geth, nethermind, erigon). If matched, returns
    /// `Recoverable::NonceTooLow` — the nonce was already consumed and we should
    /// wait for the provider to update its nonce counter before retrying.
    ///
    /// Falls back to `Transient` when the error doesn't match a known pattern,
    /// since a generic transport error may also indicate a temporary RPC failure.
    pub fn classify_send_raw_error(err: anyhow::Error) -> Self {
        let msg = err.to_string().to_lowercase();
        if msg.contains("nonce too low")
            || msg.contains("nonce has already been used")
            || msg.contains("already known")
        {
            Self::Recoverable {
                reason: RecoverableReason::NonceTooLow,
                source: err,
            }
        } else {
            Self::Transient(err)
        }
    }
}
