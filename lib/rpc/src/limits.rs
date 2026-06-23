use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{DefaultDirectRateLimiter, NotUntil, Quota, RateLimiter};
use std::collections::HashMap;
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::interval;

/// Rate-limit spec consumed by [`Limiter`] at construction.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub global_rps: Option<NonZeroU32>,
    pub methods: HashMap<String, NonZeroU32>,
}

fn bucket(rps: NonZeroU32) -> DefaultDirectRateLimiter {
    RateLimiter::direct(Quota::per_second(rps))
}

fn retry_after(not_until: NotUntil<QuantaInstant>) -> u64 {
    let now = DefaultClock::default().now();
    not_until
        .wait_time_from(now)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Stateful enforcer for a [`Limits`] spec. Owns the token buckets; middleware calls `check`
/// per request to gate it.
pub struct Limiter {
    global: Option<DefaultDirectRateLimiter>,
    per_method: HashMap<String, DefaultDirectRateLimiter>,
}

impl Limiter {
    pub fn new(limits: Limits) -> Self {
        let global = limits.global_rps.map(bucket);
        let per_method = limits
            .methods
            .into_iter()
            .map(|(name, rps)| (name, bucket(rps)))
            .collect();
        Self { global, per_method }
    }

    fn check_global(&self) -> Option<u64> {
        self.global.as_ref()?.check().err().map(retry_after)
    }

    fn check_per_method(&self, name: &str) -> Option<u64> {
        self.per_method.get(name)?.check().err().map(retry_after)
    }

    pub fn check(&self, method: &str) -> Option<u64> {
        self.check_global()
            .or_else(|| self.check_per_method(method))
    }
}

/// Wraps a [`Limiter`] with a rolling rejection counter, drained into a 1/s log line.
pub struct LoggingLimiter {
    inner: Limiter,
    rejections: AtomicU64,
}

impl LoggingLimiter {
    pub fn new(inner: Limiter) -> Arc<Self> {
        Arc::new(Self {
            inner,
            rejections: AtomicU64::new(0),
        })
    }

    pub(crate) fn check(&self, method: &str) -> Option<u64> {
        self.inner.check(method).inspect(|_| {
            self.rejections.fetch_add(1, Ordering::Relaxed);
        })
    }

    pub async fn run(this: Arc<Self>) -> Infallible {
        let mut ticker = interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let count = this.rejections.swap(0, Ordering::Relaxed);
            if count > 0 {
                tracing::warn!(count, "rpc requests rate-limited in last 1s");
            }
        }
    }
}
