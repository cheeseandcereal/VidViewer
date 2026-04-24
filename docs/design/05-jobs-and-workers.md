# 05 — Jobs and workers

Background work (metadata probing, thumbnail generation, preview sheets) runs asynchronously
in two worker lanes backed by the `jobs` table.

## Job kinds

| Kind | Purpose | Depends on |
|---|---|---|
| `probe` | Run `ffprobe`; fill `duration_secs`, `width`, `height`, `codec`. | — |
| `thumbnail` | Generate poster JPEG at `cache/thumbs/<video_id>.jpg`. | `probe` (needs duration). |
| `preview` | Generate tile sheet + WebVTT at `cache/previews/<video_id>.{jpg,vtt}`. | `probe`. |

## Lifecycle

1. Scanner enqueues a `probe` job on new/changed videos.
2. General worker picks it up, marks `running`, runs ffprobe, writes metadata
   including `is_audio_only` and `attached_pic_stream_index`.
3. On success it marks `probe` `done` and enqueues a `thumbnail` job. A
   `preview` job is enqueued too, unless the file is audio-only or its
   duration is unknown.
4. Workers pick those up per their lane.
5. On any error, the job is marked `failed` with the error message in `error`; no automatic retry for v1.

### Audio-only files

When `probe` sets `is_audio_only = 1`:

- **Preview** is never enqueued. Audio files have no visual timeline and
  the tile-sheet pipeline would produce garbage.
- **Thumbnail** behavior depends on whether `attached_pic_stream_index` is
  populated:
  - If the probe found an attached-pic stream (embedded cover art), the
    thumbnail job extracts frame 0 of that stream via
    `ffmpeg -map 0:<N> -frames:v 1 -vf scale=<thumbnail_width>:-2 …`.
    Re-encoded to `thumbnail_width` for size consistency with video
    thumbnails.
  - Otherwise the job exits early, leaving `thumbnail_ok = 0`. The UI
    renders a static music-note placeholder for audio rows without a
    generated thumbnail.
- The scanner's cache-verification pass skips preview verification entirely
  for audio-only rows; there's nothing to recover.

## Lanes

- **General lane**: concurrency = `config.worker_concurrency` (default 10). Handles `probe` and `thumbnail`.
- **Preview lane**: concurrency = `config.preview_concurrency` (default 8). Handles `preview`.

Separating preview generation prevents long tile-sheet encodes from starving quick thumbnails.

## Scheduling

- Workers long-poll the `jobs` table every ~500 ms: `SELECT ... WHERE status='pending' ORDER BY id LIMIT 1` plus an atomic `UPDATE status='running' WHERE id=? AND status='pending'` to claim.
- No external queue; SQLite is the coordination point.
- Simple FIFO order; no priority in v1.

## Failure handling

- `failed` jobs are visible via `/debug` and in scan-status reports.
- A failed `probe` blocks enqueuing of `thumbnail`/`preview` for that video; those remain absent.
- The user can trigger a rescan for the directory; if nothing changed on disk, the scanner does not re-enqueue. A dedicated "retry failed jobs" endpoint can be added post-MVP.
- `ffprobe` runs under a 60-second wall-clock timeout. A hung ffprobe would
  otherwise leave a `probe` row in `running` forever, and the
  `idx_jobs_outstanding_unique` partial unique index would prevent a new
  probe from being enqueued for the same video. The timeout turns that into
  a normal `failed` outcome, unsticking the `(kind, video_id)` slot.

## Stuck-job watchdog

In addition to startup reconciliation, a watchdog runs alongside the workers
to rescue rows that have been stranded in `running` after their worker task
disappeared (DB write failure, panic, external process signal, etc.).
Without the watchdog, such a row would stay `running` forever and the
partial unique index on outstanding jobs would block every future enqueue
for that `(kind, video_id)` pair, so rescans would appear to silently "do
nothing" for the affected video.

The detection rule has two parts: the row must be `running`, must be older
than a small age threshold, **and** its id must not be present in the
`JobRegistry`. The registry check is the source of truth for "is a task
still alive behind this row" — a long-running ffmpeg invocation is fine
because it stays registered. The age threshold only covers the tiny
claim/register race window where `claim()` has already transitioned the row
to `running` but `registry.register(...)` hasn't run yet — a few
synchronous microseconds, no `.await` between them.

The watchdog fires in two ways:

- **Periodic** (every 30 seconds, threshold 30 seconds). A background task
  spawned alongside the worker lanes.
- **Ad-hoc on manual scan** (threshold 5 seconds). `POST /api/scan` runs
  a watchdog pass before spawning the scanner. This turns a user's manual
  rescan into an explicit "please retry" — any stuck probe is unsticked
  immediately so the scanner's re-enqueues actually land, instead of
  silently no-opping against a stale `running` row.

## Cancellation

- Each lane spawns the actual per-job work as a separate tokio task and registers
  its `AbortHandle` plus `video_id` in the shared `JobRegistry` (kept on
  `AppState::job_registry`).
- When a directory is removed (soft or hard), the HTTP handler looks up all
  `video_id`s in the directory and calls `JobRegistry::cancel_for_videos`. Each
  matching task is aborted; because every `tokio::process::Command` in
  `video_tool` is built with `kill_on_drop(true)`, the ffmpeg/ffprobe child
  process is terminated when the worker future is dropped mid-await.
- Aborted rows are deleted from the `jobs` table outright. The worker loop also
  deletes them defensively when it observes the `JoinError::cancelled`.
- `jobs` rows have no FK to `videos`; hard-remove explicitly cleans them up via
  `DELETE FROM jobs WHERE video_id IN (...)`.

## Adding a job kind

See [`../agents/adding-a-job-kind.md`](../agents/adding-a-job-kind.md).
