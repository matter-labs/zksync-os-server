use alloy::consensus::SignableTransaction;
use alloy::network::TxSigner;
use alloy::primitives::{Address, Signature};
use backon::{BackoffBuilder, ExponentialBuilder};
use std::time::Duration;

/// Retry policy applied to every Google Cloud KMS operation (client creation,
/// public key fetch, transaction signing) for all GKMS-backed keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GkmsRetryPolicy {
    /// Max attempts per operation, including the initial one.
    pub max_attempts: usize,
    /// Delay before the first retry; doubles on each subsequent retry.
    pub min_delay: Duration,
    /// Upper bound for the delay between retries.
    pub max_delay: Duration,
}

impl Default for GkmsRetryPolicy {
    fn default() -> Self {
        // Exponential 1s -> 60s
        Self {
            max_attempts: 10,
            min_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl GkmsRetryPolicy {
    /// Backoff builder for use with [`backon::Retryable`].
    pub(crate) fn exponential(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.min_delay)
            .with_max_delay(self.max_delay)
            .with_max_times(self.max_attempts.saturating_sub(1))
    }

    /// Delays to sleep between attempts; yields `max_attempts - 1` items.
    fn backoff(&self) -> impl Iterator<Item = Duration> {
        self.exponential().build()
    }
}

/// Wraps a [`TxSigner`] whose signing goes over the network (GCP KMS) and
/// retries failed sign attempts per the given policy.
pub(crate) struct RetryingSigner<S> {
    inner: S,
    policy: GkmsRetryPolicy,
    resource_name: String,
}

impl<S> RetryingSigner<S> {
    pub(crate) fn new(inner: S, policy: GkmsRetryPolicy, resource_name: String) -> Self {
        Self {
            inner,
            policy,
            resource_name,
        }
    }
}

#[async_trait::async_trait]
impl<S: TxSigner<Signature> + Send + Sync> TxSigner<Signature> for RetryingSigner<S> {
    fn address(&self) -> Address {
        self.inner.address()
    }

    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy::signers::Result<Signature> {
        let mut backoff = self.policy.backoff();
        let mut attempt = 1usize;
        loop {
            match self.inner.sign_transaction(&mut *tx).await {
                Ok(signature) => return Ok(signature),
                Err(err) => match backoff.next() {
                    Some(delay) => {
                        tracing::warn!(
                            resource_name = %self.resource_name,
                            attempt,
                            ?delay,
                            %err,
                            "GKMS transaction signing failed; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                    None => {
                        tracing::error!(
                            resource_name = %self.resource_name,
                            attempts = attempt,
                            %err,
                            "GKMS transaction signing failed; retry budget exhausted"
                        );
                        return Err(err);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::TxLegacy;
    use alloy::signers::local::PrivateKeySigner;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Delegates to a real local signer, failing the first `remaining_failures` calls.
    struct FlakySigner {
        inner: PrivateKeySigner,
        remaining_failures: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FlakySigner {
        fn new(failures: usize) -> Self {
            Self {
                inner: PrivateKeySigner::random(),
                remaining_failures: AtomicUsize::new(failures),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl TxSigner<Signature> for FlakySigner {
        fn address(&self) -> Address {
            self.inner.address()
        }

        async fn sign_transaction(
            &self,
            tx: &mut dyn SignableTransaction<Signature>,
        ) -> alloy::signers::Result<Signature> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let should_fail = self
                .remaining_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if should_fail {
                return Err(alloy::signers::Error::other(std::io::Error::other(
                    "transient GKMS failure",
                )));
            }
            self.inner.sign_transaction(tx).await
        }
    }

    fn test_policy() -> GkmsRetryPolicy {
        GkmsRetryPolicy {
            max_attempts: 5,
            min_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recovers_after_transient_failures() {
        let signer = RetryingSigner::new(FlakySigner::new(3), test_policy(), "test-key".into());
        let mut tx = TxLegacy::default();
        let signature = signer
            .sign_transaction(&mut tx)
            .await
            .expect("must succeed within the retry budget");
        assert_eq!(signer.inner.calls.load(Ordering::SeqCst), 4);
        let recovered = signature
            .recover_address_from_prehash(&tx.signature_hash())
            .unwrap();
        assert_eq!(recovered, signer.address());
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let signer = RetryingSigner::new(
            FlakySigner::new(usize::MAX),
            test_policy(),
            "test-key".into(),
        );
        let mut tx = TxLegacy::default();
        let err = signer
            .sign_transaction(&mut tx)
            .await
            .expect_err("must fail once the retry budget is exhausted");
        assert_eq!(signer.inner.calls.load(Ordering::SeqCst), 5);
        assert!(err.to_string().contains("transient GKMS failure"));
    }

    #[tokio::test(start_paused = true)]
    async fn single_attempt_policy_does_not_retry() {
        let policy = GkmsRetryPolicy {
            max_attempts: 1,
            ..test_policy()
        };
        let signer = RetryingSigner::new(FlakySigner::new(usize::MAX), policy, "test-key".into());
        let mut tx = TxLegacy::default();
        signer
            .sign_transaction(&mut tx)
            .await
            .expect_err("must fail without retrying");
        assert_eq!(signer.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = GkmsRetryPolicy {
            max_attempts: 6,
            min_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(4),
        };
        let delays: Vec<_> = policy.backoff().collect();
        assert_eq!(
            delays,
            [1, 2, 4, 4, 4].map(Duration::from_secs).to_vec(),
            "expected exponential growth capped at max_delay"
        );
    }

    #[test]
    fn default_policy_budget_is_about_five_minutes() {
        let delays: Vec<_> = GkmsRetryPolicy::default().backoff().collect();
        assert_eq!(delays.len(), 9);
        let total: Duration = delays.iter().sum();
        assert_eq!(total, Duration::from_secs(1 + 2 + 4 + 8 + 16 + 32 + 60 * 3));
    }
}
