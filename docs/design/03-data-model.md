# 03 — Data model

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
    removed   INTEGER NOT NULL DEFAULT 0    -- 0/1, CHECK-constrained
        CHECK (removed IN (0, 1))
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
    thumbnail_ok   INTEGER NOT NULL DEFAULT 0 CHECK (thumbnail_ok IN (0, 1)),
    preview_ok     INTEGER NOT NULL DEFAULT 0 CHECK (preview_ok IN (0, 1)),
    missing        INTEGER NOT NULL DEFAULT 0 CHECK (missing IN (0, 1)),
    is_audio_only  INTEGER NOT NULL DEFAULT 0 CHECK (is_audio_only IN (0, 1)),
    attached_pic_stream_index INTEGER,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL,
    UNIQUE(directory_id, relative_path)
);
CREATE INDEX idx_videos_updated_at ON videos(updated_at);
CREATE INDEX idx_videos_missing    ON videos(missing);
```

`is_audio_only = 1` means the file has no real video stream (either it's a
pure audio container, or it's a video container carrying only audio plus an
optional still-image attached-pic). Set at probe time by ffprobe classification.
`attached_pic_stream_index`, when non-NULL, is the zero-based index of a
still-image stream (typically embedded cover art) that the thumbnail job can
extract as the poster image. Both columns are added by
[`migrations/0002_audio_support.sql`](../../migrations/0002_audio_support.sql).

### `collections`

```sql
CREATE TABLE collections (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('directory', 'custom')),
    directory_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
    hidden       INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE(kind, directory_id)
);
CREATE INDEX idx_collections_hidden ON collections(hidden);
```

### `collection_directories`

Directories included in a custom collection. Directory collections do not use
this table; their video membership is implicit (`videos.directory_id =
collections.directory_id`). Custom collections read their videos as the union
of videos in these directories, computed on every read — there is no
materialized membership table.

```sql
CREATE TABLE collection_directories (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    directory_id  INTEGER NOT NULL REFERENCES directories(id) ON DELETE CASCADE,
    added_at      TEXT    NOT NULL,
    PRIMARY KEY (collection_id, directory_id)
);
CREATE INDEX idx_colldirs_directory ON collection_directories(directory_id);
```

### `watch_history`

```sql
CREATE TABLE watch_history (
    video_id        TEXT PRIMARY KEY REFERENCES videos(id) ON DELETE CASCADE,
    last_watched_at TEXT    NOT NULL,
    position_secs   REAL    NOT NULL DEFAULT 0,
    completed       INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    watch_count     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_last_watched ON watch_history(last_watched_at DESC);
```

### `jobs`

```sql
CREATE TABLE jobs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL CHECK (kind IN ('probe', 'thumbnail', 'preview')),
    video_id    TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    error       TEXT,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);
CREATE INDEX idx_jobs_status   ON jobs(status);
CREATE INDEX idx_jobs_video_id ON jobs(video_id);

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
- Collection membership is computed on read, never materialized. A directory
  collection's videos are those with `videos.directory_id =
  collections.directory_id`; a custom collection's videos are the union of
  videos in the directories listed in `collection_directories`. Videos flagged
  `missing = 1` are excluded from all collection listings.
- Soft-removed directories (`removed = 1`) are skipped by the scanner; their
  directory collection has `hidden = 1`. Any `collection_directories` rows
  linking a soft-removed directory to a custom collection remain intact, so
  re-adding the directory restores its contribution to those collections
  automatically.
- Watch history is preserved under soft-remove. Hard-remove (explicit user
  action via the `mode=hard` API path) cascades through FKs: deleting a
  `directories` row removes its `videos`, which cascade to `watch_history`, the
  directory's own `collections` row, and any `collection_directories` rows
  referencing that directory. `jobs` rows have no FK and are cleaned up
  manually by the hard-remove implementation.

## Migrations

Migrations live in `migrations/NNNN_description.sql` and are **append-only**.
See [`14-migrations.md`](./14-migrations.md) for the pre-migration backup behavior.
