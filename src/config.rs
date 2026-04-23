//! Configuration loader.
//!
//! Reads `~/.config/vidviewer/config.toml` (or an explicit path via CLI / env). Writes a
//! default on first run. Paths support a leading `~` which is expanded to `$HOME`.
//!
//! Config is read once at startup; changes require a restart.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level configuration, as stored in `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,

    pub player: String,
    pub player_args: Vec<String>,

    pub thumbnail_width: u32,

    pub preview_min_interval: f64,
    pub preview_target_count: u32,

    pub worker_concurrency: u32,
    pub preview_concurrency: u32,

    pub scan_on_startup: bool,

    pub backup_before_migration: bool,
    pub backup_dir: PathBuf,

    pub enable_debug_endpoint: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7878,
            player: "mpv".into(),
            player_args: vec!["--force-window=yes".into()],
            thumbnail_width: 320,
            preview_min_interval: 2.0,
            preview_target_count: 100,
            worker_concurrency: 3,
            preview_concurrency: 1,
            scan_on_startup: true,
            backup_before_migration: true,
            backup_dir: default_backup_dir(),
            enable_debug_endpoint: false,
        }
    }
}

fn default_backup_dir() -> PathBuf {
    data_dir().join("backups")
}

/// `~/.local/share/vidviewer`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = dirs::data_local_dir() {
        d.join("vidviewer")
    } else {
        PathBuf::from(".vidviewer")
    }
}

/// `~/.config/vidviewer`.
pub fn config_dir() -> PathBuf {
    if let Some(d) = dirs::config_dir() {
        d.join("vidviewer")
    } else {
        PathBuf::from(".vidviewer-config")
    }
}

/// The default `config.toml` path.
pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Database file path. Not configurable in v1.
pub fn database_path() -> PathBuf {
    data_dir().join("vidviewer.db")
}

/// Thumbnail cache directory. Not configurable in v1.
pub fn thumb_cache_dir() -> PathBuf {
    data_dir().join("cache").join("thumbs")
}

/// Preview cache directory. Not configurable in v1.
pub fn preview_cache_dir() -> PathBuf {
    data_dir().join("cache").join("previews")
}

/// Load config from `path`. If the file does not exist, write a default and return it.
pub fn load_or_create(path: &Path) -> Result<Config> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    } else {
        let cfg = Config::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(&cfg).context("serializing default config to TOML")?;
        std::fs::write(path, text)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        tracing::info!(path = %path.display(), "wrote default config file");
        Ok(cfg)
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.port, parsed.port);
        assert_eq!(cfg.player, parsed.player);
    }

    #[test]
    fn load_creates_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vidviewer").join("config.toml");
        let cfg = load_or_create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.port, 7878);
    }

    #[test]
    fn expand_tilde_home() {
        let home = dirs::home_dir().unwrap();
        let expanded = expand_tilde(Path::new("~/stuff"));
        assert_eq!(expanded, home.join("stuff"));
    }
}
