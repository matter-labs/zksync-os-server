//! Shared retry helper for transient failures of network calls.

use crate::metrics::REPLAY_ARCHIVE_METRICS;
use std::fmt;
use std::future::Future;
use std::time::Duration;

/// Total number of attempts (the initial call plus retries) for transient failures.
pub(crate) const RETRY_ATTEMPTS: u32 = 4;
pub(crate) const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// Retries `operation` on errors accepted by `is_transient` with exponential backoff.
///
/// `operation_label` is a low-cardinality label for the `transient_retries` metric;
/// `description` is free-form text for the retry log line.
pub(crate) async fn with_transient_retries<T, E, Fut>(
    operation_label: &'static str,
    description: &str,
    is_transient: impl Fn(&E) -> bool,
    operation: impl Fn() -> Fut,
) -> Result<T, E>
where
    E: fmt::Display,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < RETRY_ATTEMPTS && is_transient(&err) => {
                REPLAY_ARCHIVE_METRICS.transient_retries[&operation_label].inc();
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %err,
                    "transient error while {description}; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn run_with_failures(failures: u32, transient: bool) -> (Result<u32, String>, u32) {
        let calls = AtomicU32::new(0);
        let result = with_transient_retries(
            "test_op",
            "testing",
            |_err: &String| transient,
            || {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if call <= failures {
                        Err(format!("failure #{call}"))
                    } else {
                        Ok(call)
                    }
                }
            },
        )
        .await;
        (result, calls.load(Ordering::SeqCst))
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_errors_until_success() {
        let (result, calls) = run_with_failures(2, true).await;

        assert_eq!(result, Ok(3));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let (result, calls) = run_with_failures(u32::MAX, true).await;

        assert_eq!(result, Err(format!("failure #{RETRY_ATTEMPTS}")));
        assert_eq!(calls, RETRY_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_permanent_errors() {
        let (result, calls) = run_with_failures(u32::MAX, false).await;

        assert_eq!(result, Err("failure #1".to_owned()));
        assert_eq!(calls, 1);
    }

    // Relies on nextest's process-per-test isolation: the global metric is not shared with
    // other tests.
    #[tokio::test(start_paused = true)]
    async fn counts_retries_in_metrics() {
        let (result, _) = run_with_failures(2, true).await;

        assert_eq!(result, Ok(3));
        assert_eq!(
            crate::metrics::REPLAY_ARCHIVE_METRICS.transient_retries[&"test_op"].get(),
            2
        );
    }
}
