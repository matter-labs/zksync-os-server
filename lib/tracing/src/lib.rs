//!  The `tracing` module provides functionalities for setting up and configuring logging.

mod formatter;
pub use formatter::LogFormat;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

///  Configuration for node's global tracer.
///
///  This struct holds configuration parameters for a tracing layer, including
///  the format, default directive and coloring.
#[derive(Debug, Clone)]
pub struct Tracer {
    format: LogFormat,
    use_color: bool,
}

impl Tracer {
    ///  Constructs a new `Tracer`.
    ///
    ///  # Arguments
    ///  * `format` - Specifies the format for log messages. Possible values are:
    ///      - `LogFormat::Json` for JSON formatting.
    ///      - `LogFormat::LogFmt` for logfmt (key=value) formatting.
    ///      - `LogFormat::Terminal` for human-readable, terminal-friendly formatting.
    ///  * `use_color` - Whether to use color for the log messages.
    pub const fn new(format: LogFormat, use_color: bool) -> Self {
        Self { format, use_color }
    }
}

impl Default for Tracer {
    ///  Provides default values for `Tracer`.
    ///
    ///  By default it uses terminal format and enabled colored logs.
    fn default() -> Self {
        Self {
            format: LogFormat::Terminal,
            use_color: true,
        }
    }
}

impl Tracer {
    pub fn init(self) {
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy();
        let layer = self.format.apply(filter, self.use_color);

        // The error is returned if the global default subscriber is already set,
        // so it's safe to ignore it
        let _ = tracing_subscriber::registry().with(layer).try_init();
    }
}
