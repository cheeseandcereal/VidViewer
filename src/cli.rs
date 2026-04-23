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
        None => {
            let db_path = crate::config::database_path();
            let pool = crate::db::init(&cfg, &db_path).await?;
            let state = crate::state::AppState::new(cfg, pool);
            crate::http::serve(state).await
        }
        Some(Command::Doctor) => doctor(&cfg).await,
        Some(Command::Scan { dry_run, dir_id }) => scan(&cfg, dry_run, dir_id).await,
    }
}

async fn doctor(cfg: &crate::config::Config) -> Result<()> {
    use std::process::Command;
    tracing::info!(port = cfg.port, "vidviewer doctor");

    let mut problems = 0u32;

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
                problems += 1;
                tracing::error!(
                    binary = label,
                    code = ?out.status.code(),
                    "binary returned non-zero status"
                );
            }
            Err(err) => {
                problems += 1;
                tracing::error!(binary = label, error = %err, "binary not found on PATH");
            }
        }
    }

    let db_path = config::database_path();
    tracing::info!(path = %db_path.display(), "database path");
    for (label, path) in [
        ("thumb cache", config::thumb_cache_dir()),
        ("preview cache", config::preview_cache_dir()),
    ] {
        match std::fs::create_dir_all(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "{label} writable"),
            Err(err) => {
                problems += 1;
                tracing::error!(path = %path.display(), error = %err, "{label} not writable");
            }
        }
    }

    match crate::db::init(cfg, &db_path).await {
        Ok(_) => tracing::info!("database ok"),
        Err(err) => {
            problems += 1;
            tracing::error!(error = format!("{err:#}"), "database init failed");
        }
    }

    if problems > 0 {
        anyhow::bail!("doctor found {problems} problem(s)");
    }
    tracing::info!("all checks passed");
    Ok(())
}

async fn scan(cfg: &crate::config::Config, dry_run: bool, dir_id: Option<i64>) -> Result<()> {
    let db_path = crate::config::database_path();
    let pool = crate::db::init(cfg, &db_path).await?;
    let clock = crate::clock::system();
    let only = dir_id.map(crate::ids::DirectoryId);

    if dry_run {
        let report = crate::scanner::dry_run_report(&pool, only).await?;
        tracing::info!(
            seen = report.seen_files,
            inserts = report.would_insert.len(),
            updates = report.would_update.len(),
            missings = report.would_mark_missing.len(),
            "dry-run summary"
        );
        for p in report.would_insert.iter().take(20) {
            tracing::info!(action = "would_insert", path = %p);
        }
        for p in report.would_update.iter().take(20) {
            tracing::info!(action = "would_update", path = %p);
        }
        for p in report.would_mark_missing.iter().take(20) {
            tracing::info!(action = "would_mark_missing", path = %p);
        }
        Ok(())
    } else {
        let report = crate::scanner::scan_all(&pool, &clock).await?;
        tracing::info!(
            dirs = report.directories_scanned,
            files = report.files_seen,
            new = report.new_videos,
            changed = report.changed_videos,
            missing = report.missing_videos,
            "scan complete"
        );
        Ok(())
    }
}
