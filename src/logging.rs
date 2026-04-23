//! `tracing` initialization.
//!
//! Pretty format by default. JSON when `LOG_FORMAT=json` or `--log-format json` is passed.
//! Log level is controlled by the `LOG_LEVEL` env var (defaults to `info`).

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

impl LogFormat {
    pub fn from_env_or(default: LogFormat) -> LogFormat {
        match std::env::var("LOG_FORMAT")
            .ok()
            .as_deref()
            .map(|v| v.to_ascii_lowercase())
        {
            Some(ref s) if s == "json" => LogFormat::Json,
            Some(ref s) if s == "pretty" => LogFormat::Pretty,
            _ => default,
        }
    }
}

/// Install a `tracing` subscriber. Safe to call at most once; subsequent calls return an
/// error (installing two global subscribers is not supported).
pub fn init(format: LogFormat) -> Result<()> {
    let filter = EnvFilter::try_from_env("LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Pretty => registry.with(fmt::layer().with_target(false)).try_init(),
        LogFormat::Json => registry
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .try_init(),
    }
    .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))
}
