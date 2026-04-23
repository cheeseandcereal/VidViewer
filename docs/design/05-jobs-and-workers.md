# 05 — Jobs and workers

Last updated: 2026-04-22

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
2. General worker picks it up, marks `running`, runs ffprobe, writes metadata.
3. On success it marks `probe` `done` and enqueues `thumbnail` and `preview` jobs.
4. Workers pick those up per their lane.
5. On any error, the job is marked `failed` with the error message in `error`; no automatic retry for v1.

## Lanes

- **General lane**: concurrency = `config.worker_concurrency` (default 3). Handles `probe` and `thumbnail`.
- **Preview lane**: concurrency = `config.preview_concurrency` (default 1). Handles `preview`.

Separating preview generation prevents long tile-sheet encodes from starving quick thumbnails.

## Scheduling

- Workers long-poll the `jobs` table every ~500 ms: `SELECT ... WHERE status='pending' ORDER BY id LIMIT 1` plus an atomic `UPDATE status='running' WHERE id=? AND status='pending'` to claim.
- No external queue; SQLite is the coordination point.
- Simple FIFO order; no priority in v1.

## Failure handling

- `failed` jobs are visible via `/debug` and in scan-status reports.
- A failed `probe` blocks enqueuing of `thumbnail`/`preview` for that video; those remain absent.
- The user can trigger a rescan for the directory; if nothing changed on disk, the scanner does not re-enqueue. A dedicated "retry failed jobs" endpoint can be added post-MVP.

## Adding a job kind

See [`../agents/adding-a-job-kind.md`](../agents/adding-a-job-kind.md).
