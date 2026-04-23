# 03 — Data model

Last updated: 2026-04-22

All application state lives in SQLite at `~/.local/share/vidviewer/vidviewer.db`.

`PRAGMA encoding = 'UTF-8'` is set at DB init.

## Tables

### `directories`

```sql
CREATE TABLE directories (
    id        INTEGER PRIMARY KEY,
    path      TEXT    NOT NULL UNIQUE,      -- absolute path
    label     TEXT    NOT NULL,             -- defaults to path, user-editable
    added_at  TEXT    NOT NULL,             -- RFC3339 UTC
    removed   INTEGER NOT NULL DEFAULT 0    -- soft-remove flag
);
CREATE INDEX idx_directories_removed ON directories(removed);
```

### `videos`

```sql
CREATE TABLE videos (
    id             TEXT PRIMARY KEY,        -- uuid
    directory_id   INTEGER NOT NULL REFERENCES directories(id) ON DELETE CASCADE,
    relative_path  TEXT    NOT NULL,
    filename       TEXT    NOT NULL,
    size_bytes     INTEGER NOT NULL,
    mtime_unix     INTEGER NOT NULL,
    duration_secs  REAL,
    width          INTEGER,
    height         INTEGER,
    codec          TEXT,
    thumbnail_ok   INTEGER NOT NULL DEFAULT 0,
    preview_ok     INTEGER NOT NULL DEFAULT 0,
    missing        INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL,
    UNIQUE(directory_id, relative_path)
);
CREATE INDEX idx_videos_updated_at ON videos(updated_at);
CREATE INDEX idx_videos_missing    ON videos(missing);
```

### `collections`

```sql
CREATE TABLE collections (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    kind         TEXT    NOT NULL,          -- 'directory' | 'custom'
    directory_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
    hidden       INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE(kind, directory_id)
);
CREATE INDEX idx_collections_hidden ON collections(hidden);
```

### `collection_videos`

Materialized membership for both directory and custom collections.
The scanner maintains rows for directory collections; users maintain rows for custom collections.

```sql
CREATE TABLE collection_videos (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    video_id      TEXT    NOT NULL REFERENCES videos(id)      ON DELETE CASCADE,
    added_at      TEXT    NOT NULL,
    position      INTEGER,                  -- nullable; for future manual ordering
    PRIMARY KEY (collection_id, video_id)
);
CREATE INDEX idx_collvids_video ON collection_videos(video_id);
```

### `watch_history`

```sql
CREATE TABLE watch_history (
    video_id        TEXT PRIMARY KEY REFERENCES videos(id) ON DELETE CASCADE,
    last_watched_at TEXT    NOT NULL,
    position_secs   REAL    NOT NULL DEFAULT 0,
    completed       INTEGER NOT NULL DEFAULT 0,
    watch_count     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_last_watched ON watch_history(last_watched_at DESC);
```

### `jobs`

```sql
CREATE TABLE jobs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,           -- 'probe' | 'thumbnail' | 'preview'
    video_id    TEXT    NOT NULL,
    status      TEXT    NOT NULL,           -- 'pending' | 'running' | 'done' | 'failed'
    error       TEXT,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);
CREATE INDEX idx_jobs_status ON jobs(status);

-- At most one outstanding (pending or running) job per (kind, video_id).
-- Enforced at the DB layer as a safety net for the idempotent enqueue path.
CREATE UNIQUE INDEX idx_jobs_outstanding_unique
    ON jobs(kind, video_id)
    WHERE status IN ('pending', 'running');
```

### `ui_state`

Single-row table holding small UI state that should persist across restarts (e.g. the last
directory browsed in the directory picker).

```sql
CREATE TABLE ui_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    last_browsed_path TEXT
);
```

## Invariants

- `(directory_id, relative_path)` is the scanner's identity key for videos.
- `video_id` is stable across file content changes. URLs to derived assets include `?v=<updated_at_epoch>` to bust caches.
- Directory collections (`kind = 'directory'`) can only be mutated through their `name` field.
- When a video is soft-deleted (`missing = 1`) its `collection_videos` row for its directory collection is deleted; custom collection rows are preserved.
- Soft-removed directories (`removed = 1`) are skipped by the scanner; their directory collection has `hidden = 1`.
- Watch history is preserved under soft-remove. Hard-remove (explicit user action
  via the `mode=hard` API path) cascades through FKs: deleting a `directories`
  row removes its `videos`, which in turn cascades to `collection_videos`,
  `watch_history`, and the directory's own `collections` row. `jobs` rows have
  no FK and are cleaned up manually by the hard-remove implementation.

## Migrations

Migrations live in `migrations/NNNN_description.sql` and are **append-only**.
See [`14-migrations.md`](./14-migrations.md) for the pre-migration backup behavior.
