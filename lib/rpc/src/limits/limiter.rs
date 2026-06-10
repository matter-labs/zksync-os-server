use super::Policy;
use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{DefaultDirectRateLimiter, NotUntil, Quota};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

fn limiter(rps: NonZeroU32) -> DefaultDirectRateLimiter {
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

/// Stateful enforcer for a [`Policy`]. Owns the token buckets; middleware calls `check` per request to gate it.
pub struct Limiter {
    global: Option<DefaultDirectRateLimiter>,
    per_method: HashMap<String, DefaultDirectRateLimiter>,
}

impl Limiter {
    pub fn new(policy: Arc<dyn Policy>) -> Arc<Self> {
        let global = policy.global().map(limiter);
        let per_method = policy
            .methods()
            .into_iter()
            .map(|(name, rps)| (name, limiter(rps)))
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
