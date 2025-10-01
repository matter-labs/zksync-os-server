use std::borrow::Cow;

pub fn init_sentry(url: &str) -> sentry::ClientInitGuard {
    let options = sentry::ClientOptions {
        release: Some(Cow::from(crate::metadata::NODE_VERSION)),
        environment: std::env::var("POD_NAMESPACE").ok().map(Cow::from),
        attach_stacktrace: true,
        // maybe should be revisited in the future
        traces_sample_rate: 1.0,
        ..Default::default()
    };

    sentry::init((url, options))
}
