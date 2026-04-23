# 02 — Architecture

Last updated: 2026-04-22

## Component diagram

```
┌─────────────────────────────────────────────────────────┐
│                      Browser (UI)                        │
│   Server-rendered HTML (Askama) + vanilla JS fetch       │
└────────────────────────┬────────────────────────────────┘
                         │ HTTP (localhost)
┌────────────────────────▼────────────────────────────────┐
│                    Rust Axum Server                      │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐  │
│  │  HTTP API   │  │   Scanner    │  │  Player Ctrl   │  │
│  │  + Static   │  │   (tokio     │  │ (mpv spawn +   │  │
│  │   Files     │  │    tasks)    │  │  IPC socket)   │  │
│  └──────┬──────┘  └──────┬───────┘  └────────┬───────┘  │
│         │                │                    │          │
│  ┌──────▼────────────────▼────────────────────▼───────┐ │
│  │         SQLite (sqlx) + thumbnail/preview cache    │ │
│  └────────────────────────────────────────────────────┘ │
└───────────────────┬──────────────┬──────────────────────┘
                    │              │
           ┌────────▼───┐    ┌─────▼────┐
           │  ffmpeg    │    │   mpv    │
           │ subprocess │    │ subprocess│
           └────────────┘    └──────────┘
```

## Process model

A single Rust process runs:

- **HTTP server** (`axum` + `tower-http`) handling pages and API.
- **Scanner** as a `tokio` task; triggered at startup and on demand.
- **Job workers**:
  - General lane (default concurrency 10): `probe`, `thumbnail`.
  - Preview lane (default concurrency 8): `preview`.
- **Player sessions**: one `tokio` task per active `mpv` IPC socket.

All share a single `SqlitePool` and a single `AppState` value.

Subprocesses:

- `ffmpeg` / `ffprobe`: short-lived, spawned per job. Paths passed via `Command::arg(&Path)`.
- `mpv`: long-lived per play session. Connected to via Unix socket at `/tmp/vidviewer-mpv-<uuid>.sock`.

## Request flow (typical page render)

1. Browser issues `GET /collections/:id`.
2. Axum router dispatches to a page handler in `src/http/pages.rs`.
3. Handler queries SQLite (via `sqlx`) for collection metadata and member videos.
4. Handler renders an Askama template into `text/html; charset=utf-8`.
5. Browser loads referenced `/thumbs/:id.jpg` and `/previews/:id.jpg|.vtt` assets served from disk cache.

## Request flow (play)

1. Browser `POST /api/videos/:id/play?start=...`.
2. Handler looks up resume position (unless `start=` provided), spawns `mpv` via the `Player` trait.
3. Background task connects to the IPC socket, subscribes to `time-pos`, and persists progress.
4. On session end, `watch_history` is finalized (`completed` when ≥ 90% watched).

## Modules

```
src/
├── main.rs
├── config/              # config.toml loader
├── db/                  # SqlitePool, migrations, pre-migration backup
├── ids.rs               # VideoId/CollectionId/DirectoryId newtypes
├── clock.rs             # Clock trait
├── scanner/             # stat-based detection, dir-collection materialization
├── jobs/
│   ├── mod.rs
│   ├── probe.rs
│   ├── thumbnail.rs
│   └── preview.rs
├── collections/
├── videos/
├── history/
├── player/              # Player trait + MpvPlayer
├── video_tool/          # VideoTool trait + FfmpegTool
├── http/
│   ├── mod.rs
│   ├── api.rs
│   ├── pages.rs
│   ├── debug.rs
│   └── routes.rs
├── util/
│   └── url.rs
└── cli/                 # doctor, scan --dry-run subcommands
```

See the individual design docs for subsystem details.
