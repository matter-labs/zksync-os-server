use std::borrow::Cow;

pub fn init_sentry(url: &str) -> sentry::ClientInitGuard {
    let options = sentry::ClientOptions {
        release: Some(Cow::from(crate::metadata::NODE_VERSION)),
        environment: Some(Cow::from(
            std::env::var("POD_NAMESPACE").unwrap_or("unknown/localhost".to_string()),
        )),
        attach_stacktrace: true,
        traces_sample_rate: 1.0,
        ..Default::default()
    };

    sentry::init((url, options))
}
