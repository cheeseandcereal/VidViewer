-- 0001_initial.sql
-- Initial schema for VidViewer.
--
-- Append-only: never edit this file after commit.
-- See docs/design/03-data-model.md for a narrative description.

PRAGMA foreign_keys = ON;

CREATE TABLE directories (
    id        INTEGER PRIMARY KEY,
    path      TEXT    NOT NULL UNIQUE,
    label     TEXT    NOT NULL,
    added_at  TEXT    NOT NULL,
    removed   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_directories_removed ON directories(removed);

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
    thumbnail_ok   INTEGER NOT NULL DEFAULT 0,
    preview_ok     INTEGER NOT NULL DEFAULT 0,
    missing        INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL,
    UNIQUE(directory_id, relative_path)
);
CREATE INDEX idx_videos_updated_at ON videos(updated_at);
CREATE INDEX idx_videos_missing    ON videos(missing);

CREATE TABLE collections (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('directory', 'custom')),
    directory_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
    hidden       INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE(kind, directory_id)
);
CREATE INDEX idx_collections_hidden ON collections(hidden);

CREATE TABLE collection_videos (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    video_id      TEXT    NOT NULL REFERENCES videos(id)      ON DELETE CASCADE,
    added_at      TEXT    NOT NULL,
    position      INTEGER,
    PRIMARY KEY (collection_id, video_id)
);
CREATE INDEX idx_collvids_video ON collection_videos(video_id);

CREATE TABLE watch_history (
    video_id        TEXT    PRIMARY KEY REFERENCES videos(id) ON DELETE CASCADE,
    last_watched_at TEXT    NOT NULL,
    position_secs   REAL    NOT NULL DEFAULT 0,
    completed       INTEGER NOT NULL DEFAULT 0,
    watch_count     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_last_watched ON watch_history(last_watched_at DESC);

CREATE TABLE jobs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL CHECK (kind IN ('probe', 'thumbnail', 'preview')),
    video_id    TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    error       TEXT,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);
CREATE INDEX idx_jobs_status ON jobs(status);

-- At most one outstanding (pending or running) job per (kind, video_id). This is
-- the DB-level backstop for the idempotent enqueue path in src/jobs/mod.rs.
CREATE UNIQUE INDEX idx_jobs_outstanding_unique
    ON jobs(kind, video_id)
    WHERE status IN ('pending', 'running');

CREATE TABLE ui_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    last_browsed_path TEXT
);
INSERT INTO ui_state (id, last_browsed_path) VALUES (1, NULL);
