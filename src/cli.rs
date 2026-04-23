//! CLI argument parsing and subcommand dispatch.
//!
//! `vidviewer` (no args)  → run the HTTP server.
//! `vidviewer doctor`     → environment sanity check.
//! `vidviewer scan ...`   → scanner operations (dry-run supported in v1).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{config, logging::LogFormat};

#[derive(Debug, Parser)]
#[command(
    name = "vidviewer",
    version,
    about = "Local-first video library browser"
)]
pub struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/vidviewer/config.toml)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Log output format.
    #[arg(long, value_enum, global = true)]
    pub log_format: Option<CliLogFormat>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum CliLogFormat {
    Pretty,
    Json,
}

impl From<CliLogFormat> for LogFormat {
    fn from(v: CliLogFormat) -> Self {
        match v {
            CliLogFormat::Pretty => LogFormat::Pretty,
            CliLogFormat::Json => LogFormat::Json,
        }
    }
}

impl Cli {
    pub fn log_format(&self) -> LogFormat {
        if let Some(f) = self.log_format {
            return f.into();
        }
        LogFormat::from_env_or(LogFormat::Pretty)
    }

    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(config::default_config_path)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Environment sanity check (external binaries, paths, DB connection).
    Doctor,
    /// Scanner operations.
    Scan {
        /// Show planned changes without writing.
        #[arg(long)]
        dry_run: bool,
        /// Limit to a single directory id.
        dir_id: Option<i64>,
    },
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    let config_path = cli.config_path();
    let cfg = config::load_or_create(&config_path)?;

    match cli.command {
        None => crate::http::serve(cfg).await,
        Some(Command::Doctor) => doctor(&cfg).await,
        Some(Command::Scan { dry_run, dir_id }) => scan(&cfg, dry_run, dir_id).await,
    }
}

async fn doctor(cfg: &crate::config::Config) -> Result<()> {
    use std::process::Command;
    tracing::info!(port = cfg.port, "vidviewer doctor");

    for (label, bin) in [
        ("ffmpeg", "ffmpeg"),
        ("ffprobe", "ffprobe"),
        ("mpv", cfg.player.as_str()),
    ] {
        match Command::new(bin).arg("-version").output() {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let first = stdout.lines().next().unwrap_or("");
                tracing::info!(binary = label, version = %first, "ok");
            }
            Ok(out) => {
                tracing::error!(
                    binary = label,
                    code = ?out.status.code(),
                    "binary returned non-zero status"
                );
            }
            Err(err) => {
                tracing::error!(binary = label, error = %err, "binary not found on PATH");
            }
        }
    }

    tracing::info!(path = %config::database_path().display(), "database path");
    tracing::info!(path = %config::thumb_cache_dir().display(), "thumb cache");
    tracing::info!(path = %config::preview_cache_dir().display(), "preview cache");

    Ok(())
}

async fn scan(_cfg: &crate::config::Config, _dry_run: bool, _dir_id: Option<i64>) -> Result<()> {
    tracing::warn!("scan subcommand not yet implemented (see docs/plan/mvp-build-order.md)");
    Ok(())
}
