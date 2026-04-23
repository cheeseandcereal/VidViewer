# 12 — Configuration

Last updated: 2026-04-22

Config lives at `~/.config/vidviewer/config.toml`. On first run the app writes a default
file if none exists. Paths in keys that accept `~` are expanded to `$HOME`.

## Full example

```toml
# Server
port = 7878

# External player
player      = "mpv"
player_args = ["--force-window=yes"]

# Thumbnails
thumbnail_width = 320

# Previews (hover-scrub tile sheets)
preview_min_interval = 2       # minimum seconds between previews
preview_target_count = 100     # max previews per video

# Workers
worker_concurrency  = 10       # probe + thumbnail
preview_concurrency = 8        # preview

# Scanner
scan_on_startup = true

# Backups (taken before any migration runs)
backup_before_migration = true
backup_dir              = "~/.local/share/vidviewer/backups"

# Introspection
enable_debug_endpoint = false
```

## Paths not configurable (v1)

- Database file: `~/.local/share/vidviewer/vidviewer.db`
- Cache root: `~/.local/share/vidviewer/cache/`

These are fixed to keep the layout predictable for backup tooling.

## Environment overrides

- `LOG_FORMAT=json` switches log output to JSON.
- `LOG_LEVEL=<level>` overrides the default `info` level (`trace`, `debug`, `info`, `warn`, `error`).

## Reload

Config is read once at startup. Changes require a restart.
