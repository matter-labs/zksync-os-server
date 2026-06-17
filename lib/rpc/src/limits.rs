use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{DefaultDirectRateLimiter, NotUntil, Quota};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

/// Rate-limit spec consumed by [`Limiter`] at construction.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub global_rps: Option<NonZeroU32>,
    pub methods: HashMap<String, NonZeroU32>,
}

fn bucket(rps: NonZeroU32) -> DefaultDirectRateLimiter {
    governor::RateLimiter::direct(Quota::per_second(rps))
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
    pub fn new(limits: Limits) -> Arc<Self> {
        let global = limits.global_rps.map(bucket);
        let per_method = limits
            .methods
            .into_iter()
            .map(|(name, rps)| (name, bucket(rps)))
            .collect();
        Arc::new(Self { global, per_method })
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
