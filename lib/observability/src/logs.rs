use std::{backtrace::Backtrace, fmt::Display};

use serde::{Deserialize, Serialize};
use tracing_subscriber::{EnvFilter, Layer, fmt, registry::LookupSpan};

/// Represents the logging format.
///
/// This enum defines the supported formats for logging output.
/// It is used to configure the format layer of a tracing subscriber.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Represents JSON formatting for logs.
    /// This format outputs log records as JSON objects,
    /// making it suitable for structured logging.
    Json,

    /// Represents logfmt (key=value) formatting for logs.
    /// This format is concise and human-readable,
    /// typically used in command-line applications.
    LogFmt,

    /// Represents terminal-friendly formatting for logs.
    #[default]
    Terminal,
}

impl Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json => write!(f, "json"),
            Self::LogFmt => write!(f, "logfmt"),
            Self::Terminal => write!(f, "terminal"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Logs {
    format: LogFormat,
    log_directives: Option<String>,
    disable_default_logs: bool,
    use_color: bool,
}

impl Logs {
    pub fn new(format: LogFormat, use_color: bool) -> Self {
        Self {
            format,
            log_directives: None,
            disable_default_logs: false,
            use_color,
        }
    }

    /// Builds a filter for the logs.
    ///
    /// Unless `disable_default_logs` was set, uses `zksync=info` as a default which is then merged
    /// with user-defined directives. Provided directives can extend/override the default value.
    ///
    /// The provided default enables `debug` level logging for all the crates with a name starting with `zksync`
    /// (per `tracing` [documentation][1]), and `info` for everything else, which is a good enough default for any project.
    ///
    /// If `log_directives` are provided via `with_log_directives`, they will be used.
    /// Otherwise, the value will be parsed from the environment variable `RUST_LOG`.
    ///
    /// [1]: https://docs.rs/tracing-subscriber/0.3.18/tracing_subscriber/filter/targets/struct.Targets.html#filtering-with-targets
    pub(super) fn build_filter(&self) -> EnvFilter {
        let mut directives = if self.disable_default_logs {
            "".to_string()
        } else {
            "INFO,\
            zksync_os_server=DEBUG,\
            zksync_os_server::prover_api=DEBUG,\
            zksync_os_sequencer=DEBUG,\
            zksync_os_priority_tree=DEBUG,\
            zksync_os_merkle_tree=DEBUG,\
            "
            .to_string()
        };
        if let Some(log_directives) = &self.log_directives {
            directives.push_str(log_directives);
        } else if let Ok(env_directives) = std::env::var(EnvFilter::DEFAULT_ENV) {
            directives.push_str(&env_directives);
        };
        EnvFilter::new(directives)
    }

    pub fn with_log_directives(mut self, log_directives: Option<String>) -> Self {
        self.log_directives = log_directives;
        self
    }

    pub fn disable_default_logs(mut self) -> Self {
        self.disable_default_logs = true;
        self
    }

    pub fn install_panic_hook(&self) {
        // Check whether we need to change the default panic handler.
        // Note that this must happen before we initialize Sentry, since otherwise
        // Sentry's panic handler will also invoke the default one, resulting in unformatted
        // panic info being output to stderr.
        if matches!(self.format, LogFormat::Json) {
            // Remove any existing hook. We expect that no hook is set by default.
            let _ = std::panic::take_hook();
            // Override the default panic handler to print the panic in JSON format.
            std::panic::set_hook(Box::new(json_panic_handler));
        };
    }

    pub fn into_layer<S>(self) -> impl Layer<S>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        let filter = self.build_filter();
        let layer = match self.format {
            LogFormat::LogFmt => tracing_logfmt::layer().boxed(),
            LogFormat::Terminal => tracing_subscriber::fmt::layer()
                .with_ansi(self.use_color)
                .with_target(true)
                .boxed(),
            LogFormat::Json => {
                let timer = tracing_subscriber::fmt::time::UtcTime::rfc_3339();
                let json_layer = fmt::Layer::default()
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(timer)
                    .json();
                json_layer.boxed()
            }
        };
        layer.with_filter(filter)
    }
}

#[allow(deprecated)] // Not available yet on stable, so we can't switch right now.
fn json_panic_handler(panic_info: &std::panic::PanicInfo) {
    let backtrace = Backtrace::force_capture();
    let timestamp = chrono::Utc::now();
    let panic_message = if let Some(s) = panic_info.payload().downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
        s
    } else {
        "Panic occurred without additional info"
    };

    let panic_location = panic_info
        .location()
        .map(|val| val.to_string())
        .unwrap_or_else(|| "Unknown location".to_owned());

    let backtrace_str = backtrace.to_string();
    let timestamp_str = timestamp.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string();

    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp_str,
            "level": "CRITICAL",
            "fields": {
                "message": panic_message,
                "location": panic_location,
                "backtrace": backtrace_str,
            }
        })
    );
}
