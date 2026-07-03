//! Link-quality presets shared by the scenario corpus.

use commonware_p2p::simulated::Link;
use std::time::Duration;

/// A well-behaved datacenter-ish network.
pub fn healthy() -> Link {
    Link {
        latency: Duration::from_millis(20),
        jitter: Duration::from_millis(5),
        success_rate: 1.0,
    }
}

/// A poor network: slow, jittery, dropping one message in ten.
pub fn degraded() -> Link {
    Link {
        latency: Duration::from_millis(80),
        jitter: Duration::from_millis(40),
        success_rate: 0.9,
    }
}
