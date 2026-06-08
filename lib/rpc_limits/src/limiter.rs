use crate::Policy;
use dashmap::DashMap;
use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{DefaultDirectRateLimiter, NotUntil, Quota};
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
    policy: Arc<dyn Policy>,
    global: Option<DefaultDirectRateLimiter>,
    per_method: DashMap<String, Arc<DefaultDirectRateLimiter>>,
}

impl Limiter {
    pub fn new(policy: Arc<dyn Policy>) -> Arc<Self> {
        let global = policy.global().map(limiter);
        Arc::new(Self {
            policy,
            global,
            per_method: DashMap::new(),
        })
    }

    fn method(&self, name: &str) -> Option<Arc<DefaultDirectRateLimiter>> {
        if let Some(limiter) = self.per_method.get(name) {
            return Some(limiter.clone());
        }
        let limiter = Arc::new(limiter(self.policy.method(name)?));
        self.per_method.insert(name.to_owned(), limiter.clone());
        Some(limiter)
    }

    fn check_global(&self) -> Option<u64> {
        self.global.as_ref()?.check().err().map(retry_after)
    }

    fn check_per_method(&self, name: &str) -> Option<u64> {
        self.method(name)?.check().err().map(retry_after)
    }

    pub fn check(&self, method: &str) -> Option<u64> {
        self.check_global()
            .or_else(|| self.check_per_method(method))
    }
}
