use std::{borrow::Cow, sync::Arc};

// Temporary re-export of `sentry::capture_message` aiming to simplify the transition from `vlog` to using
// crates directly.
use sentry::{
    ClientInitGuard,
    protocol::{Event, Exception, Values},
    types::Dsn,
};
pub use sentry::{Level as AlertLevel, capture_message};
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Debug)]
pub struct Sentry {
    url: Dsn,
    environment: Option<String>,
    node_version: Option<String>,
}

impl Sentry {
    pub fn new(url: &str) -> Result<Self, sentry::types::ParseDsnError> {
        Ok(Self {
            url: url.parse()?,
            environment: None,
            node_version: None,
        })
    }

    pub fn with_node_version(mut self, node_version: Option<String>) -> Self {
        self.node_version = node_version;
        self
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
            release: self.node_version.map(Cow::from),
            environment: self
                .environment
                .or_else(|| std::env::var("CLUSTER_NAME").ok())
                .map(Cow::from),
            attach_stacktrace: true,
            traces_sample_rate: 1.0,
            before_send: Some(Arc::new(|mut event: Event<'static>| {
                event.tags.insert(
                    "namespace".to_string(),
                    std::env::var("POD_NAMESPACE").unwrap_or("unknown/localhost".to_string()),
                );

                if event.exception.is_empty() {
                    if !event.level.is_error() && !event.level.is_warning() {
                        tracing::warn!(?event, "Unexpected level is used for sentry event");
                    }

                    event.exception = Values::from(vec![Exception {
                        ty: event.level.to_string(),
                        value: event.message.clone(),
                        ..Default::default()
                    }]);
                }

                Some(event)
            })),
            ..Default::default()
        };

        sentry::init((self.url, options))
    }
}
