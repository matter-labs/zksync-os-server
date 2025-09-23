// Ported from reth with some minor changes
// https://github.com/paradigmxyz/reth/blob/0ff2827a799a5a77c40b169bee07f9ca415b78d9/crates/tracing/src/formatter.rs

use serde::{Deserialize, Serialize};
use std::{fmt, fmt::Display};
use tracing_subscriber::{EnvFilter, Layer, Registry};

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

impl LogFormat {
    /// Applies the specified logging format to create a new layer.
    ///
    /// This method constructs a tracing layer with the selected format,
    /// along with additional configurations for filtering and output.
    ///
    /// # Arguments
    /// * `filter` - An `EnvFilter` used to determine which log records to output.
    /// * `use_color` - An optional string that enables or disables ANSI color codes in the logs.
    ///
    /// # Returns
    /// A boxed `Registry` layer that can be added to a tracing subscriber.
    pub fn apply(
        &self,
        filter: EnvFilter,
        use_color: bool,
    ) -> Box<dyn Layer<Registry> + Send + Sync + 'static> {
        match self {
            Self::Json => tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(use_color)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_filter(filter)
                .boxed(),
            Self::LogFmt => tracing_logfmt::layer().with_filter(filter).boxed(),
            Self::Terminal => tracing_subscriber::fmt::layer()
                .with_ansi(use_color)
                .with_target(true)
                .with_filter(filter)
                .boxed(),
        }
    }
}

impl Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json => write!(f, "json"),
            Self::LogFmt => write!(f, "logfmt"),
            Self::Terminal => write!(f, "terminal"),
        }
    }
}
