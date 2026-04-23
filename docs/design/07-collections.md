# 07 — Collections

Last updated: 2026-04-22

All video browsing happens through collections. There is no "all videos" view.

## Kinds

### Directory collections (`kind = 'directory'`)

- Auto-created when a directory is added.
- One per `directories` row (`collections.directory_id` FK).
- `name` defaults to the directory's full path and is user-editable. Editing the directory's
  `label` or the collection's `name` are equivalent operations; the API updates both.
- Membership is **materialized** in `collection_videos` and maintained by the scanner.
  - Video becomes `missing = 1` → its `collection_videos` row for the directory collection is deleted.
  - Video un-misses → the row is re-inserted.
- Any mutation beyond `name` rename returns 400.

### Custom collections (`kind = 'custom'`)

- Created, renamed, deleted, populated freely by the user.
- A video can belong to many custom collections.
- If a member video is `missing = 1`, it remains in the collection but is rendered with a
  "missing" badge and cannot be launched.

## Home page

Home (`/`) shows two sections:

- **Directories**: directory collections with `hidden = 0`.
- **My Collections**: custom collections.

Each card shows name, video count, and a small mosaic of recent thumbnails.

## Soft-removed directories

When a directory is soft-removed (`directories.removed = 1`):

- Any in-flight background jobs for videos in this directory are **aborted**:
  the worker task is cancelled via `AbortHandle`, and the ffmpeg/ffprobe child
  process is terminated via `kill_on_drop(true)` on the `Command`. Aborted job
  rows are deleted outright.
- Its directory collection is flagged `hidden = 1` and disappears from Home.
- All `collection_videos` rows for that collection are deleted.
- All videos in that directory are flagged `missing = 1`.
- `watch_history` and custom-collection memberships are preserved.
- Cached thumbnails and previews are left on disk.

When re-added:

- `directories.removed` is cleared, `collections.hidden` is cleared, and the collection's
  existing `name` is preserved (so user label edits survive re-adds).
- The scanner runs and repopulates `collection_videos`. Videos whose stat signature
  still matches the stored row have their `thumbnail_ok` / `preview_ok` flags
  preserved; the cache-verification pass re-enqueues jobs only for videos whose
  cache files are no longer on disk. See [`04-scanner.md`](./04-scanner.md).

## Hard-removed directories

Hard-remove is an explicit user action (`DELETE /api/directories/:id?mode=hard`).
It is irreversible:

- Any in-flight background jobs for videos in this directory are **aborted** and
  their child processes terminated, same as soft-remove.
- All thumbnail and preview cache files for videos in the directory are removed
  from disk (best-effort).
- All `jobs` rows referencing those videos are deleted.
- The `directories` row is deleted, which cascades via FK to `videos`,
  `collection_videos` (both directory and custom collection memberships), the
  directory's own `collections` row, and `watch_history`.
- Custom collections themselves remain; only their memberships for these videos
  are removed.

Never hard-remove as a side effect of another action. It must be user-initiated
through the `mode=hard` API path.

## Random

Each collection page has a **🎲 Random** button (hotkey `R`) that:

1. Calls `GET /api/collections/:id/random` → `{ video_id }` (uniform random over
   `videos` joined through `collection_videos`, filtered by `missing = 0`).
2. Navigates to `/videos/:video_id?cid=:id`.
3. No auto-launch; the user decides on the detail page.

Empty collection → endpoint returns 404; UI shows a message.
