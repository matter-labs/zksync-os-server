use std::{borrow::Cow, sync::Arc};

use sentry::protocol::{Event, Exception, Values};

pub fn init_sentry(url: &str) -> sentry::ClientInitGuard {
    let options = sentry::ClientOptions {
        release: Some(Cow::from(crate::metadata::NODE_VERSION)),
        environment: Some(Cow::from(
            std::env::var("POD_NAMESPACE").unwrap_or("unknown/localhost".to_string()),
        )),
        attach_stacktrace: true,
        traces_sample_rate: 1.0,
        before_send: Some(Arc::new(|mut event: Event<'static>| {
            if event.exception.is_empty() {
                let ty= match event.level {
                    sentry::Level::Error => "Error".to_string(),
                    sentry::Level::Warning => "Warning".to_string(),
                    _ => unreachable!("We should not promote other levels to sentry events")
                };

                event.exception = Values::from(vec![Exception {
                    ty,
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
