//! VidViewer — local-first video library browser.
//!
//! Entry point. Parses CLI args, loads config, initializes tracing, then either runs a
//! subcommand (e.g. `doctor`, `scan --dry-run`) or starts the HTTP server.

use std::process::ExitCode;

use clap::Parser;
use tracing::error;

use vidviewer::cli::{run_cli, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = vidviewer::logging::init(cli.log_format()) {
        eprintln!("failed to initialize logging: {err:#}");
        return ExitCode::from(2);
    }

    match run_cli(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = format!("{err:#}"), "vidviewer exited with error");
            ExitCode::FAILURE
        }
    }
}
