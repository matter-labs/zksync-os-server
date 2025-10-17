use std::{borrow::Cow, sync::Arc};

use sentry::protocol::{Event, Exception, Values};

pub fn init_sentry(url: &str) -> sentry::ClientInitGuard {
    let options = sentry::ClientOptions {
        release: Some(Cow::from(crate::metadata::NODE_VERSION)),
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

    sentry::init((url, options))
}
