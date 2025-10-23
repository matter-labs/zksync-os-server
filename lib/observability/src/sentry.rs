use std::borrow::Cow;

// Temporary re-export of `sentry::capture_message` aiming to simplify the transition from `vlog` to using
// crates directly.
use sentry::{ClientInitGuard, types::Dsn};
pub use sentry::{Level as AlertLevel, capture_message};
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Debug)]
pub struct Sentry {
    url: Dsn,
    environment: Option<String>,
}

impl Sentry {
    pub fn new(url: &str) -> Result<Self, sentry::types::ParseDsnError> {
        Ok(Self {
            url: url.parse()?,
            environment: None,
        })
    }

    pub fn with_environment(mut self, environment: Option<String>) -> Self {
        self.environment = environment;
        self
    }

    pub fn layer<S>(&self) -> impl Layer<S>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        sentry::integrations::tracing::layer()
            .event_filter(|metadata| match *metadata.level() {
                tracing::Level::ERROR => sentry::integrations::tracing::EventFilter::Event,
                tracing::Level::WARN => sentry::integrations::tracing::EventFilter::Event,
                _ => sentry::integrations::tracing::EventFilter::Ignore,
            })
            .span_filter(|metadata| {
                matches!(
                    *metadata.level(),
                    tracing::Level::ERROR | tracing::Level::WARN
                )
            })
    }

    pub fn install(self) -> ClientInitGuard {
        // Initialize the Sentry.
        let options = sentry::ClientOptions {
            release: sentry::release_name!(),
            environment: self.environment.map(Cow::from),
            attach_stacktrace: true,
            ..Default::default()
        };

        sentry::init((self.url, options))
    }
}
