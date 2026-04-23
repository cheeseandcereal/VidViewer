-- 0001_initial.sql
-- Initial schema for VidViewer.
--
-- Normally append-only; future schema changes must be added as new
-- migrations rather than edits to this file.
--
-- See docs/design/03-data-model.md for a narrative description of each table.

PRAGMA foreign_keys = ON;

-- User-configured root directories. `removed = 1` is a soft-remove state:
-- the row is skipped by the scanner but preserved so watch history and
-- custom-collection memberships survive.
CREATE TABLE directories (
    id        INTEGER PRIMARY KEY,
    path      TEXT    NOT NULL UNIQUE,
    label     TEXT    NOT NULL,
    added_at  TEXT    NOT NULL,
    removed   INTEGER NOT NULL DEFAULT 0 CHECK (removed IN (0, 1))
);
CREATE INDEX idx_directories_removed ON directories(removed);

-- Every video file ever indexed. `missing = 1` means the file is no longer
-- on disk; we keep the row so custom-collection memberships and watch history
-- survive a temporary absence (disk unmount, cache cleared, etc.).
CREATE TABLE videos (
    id             TEXT    PRIMARY KEY,
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
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL,
    UNIQUE(directory_id, relative_path)
);
CREATE INDEX idx_videos_updated_at ON videos(updated_at);
CREATE INDEX idx_videos_missing    ON videos(missing);

-- Collections are either auto-managed ('directory' kind, tied to a directories
-- row) or user-curated ('custom'). Custom collections are unions of one or
-- more directories (see collection_directories below); their video membership
-- is computed on read. `hidden = 1` is the soft-remove companion for directory
-- collections.
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

-- Directories included in a custom collection. Directory collections do not
-- use this table; their membership is implicit (videos.directory_id equals
-- collections.directory_id). Custom collections read their videos as the
-- union of videos in these directories.
CREATE TABLE collection_directories (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    directory_id  INTEGER NOT NULL REFERENCES directories(id) ON DELETE CASCADE,
    added_at      TEXT    NOT NULL,
    PRIMARY KEY (collection_id, directory_id)
);
CREATE INDEX idx_colldirs_directory ON collection_directories(directory_id);

-- One row per video with at least one playback event. `completed = 1` is set
-- when position reaches >= 90% of duration at end-of-session.
CREATE TABLE watch_history (
    video_id        TEXT    PRIMARY KEY REFERENCES videos(id) ON DELETE CASCADE,
    last_watched_at TEXT    NOT NULL,
    position_secs   REAL    NOT NULL DEFAULT 0,
    completed       INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    watch_count     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_last_watched ON watch_history(last_watched_at DESC);

-- Background work queue. See docs/design/05-jobs-and-workers.md.
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

-- At most one outstanding (pending or running) job per (kind, video_id). This
-- is the DB-level backstop for the idempotent enqueue path in src/jobs/mod.rs.
CREATE UNIQUE INDEX idx_jobs_outstanding_unique
    ON jobs(kind, video_id)
    WHERE status IN ('pending', 'running');

-- Single-row bag of small UI persistence (e.g. the last path visited in the
-- directory picker). The CHECK constraint enforces the singleton property.
CREATE TABLE ui_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    last_browsed_path TEXT
);
INSERT INTO ui_state (id, last_browsed_path) VALUES (1, NULL);
