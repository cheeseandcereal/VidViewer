# 07 — Collections

All video browsing happens through collections. There is no "all videos" view.
Collection video membership is **always computed on read** — there is no
materialized membership table.

## Kinds

### Directory collections (`kind = 'directory'`)

- Auto-created when a directory is added; one per `directories` row
  (`collections.directory_id` FK).
- `name` defaults to the directory's full path and is user-editable. Editing
  the directory's `label` or the collection's `name` are equivalent operations;
  the API updates both.
- Membership is implicit: the videos of a directory collection are those with
  `videos.directory_id = collections.directory_id` and `missing = 0`.
- Any mutation beyond `name` rename returns 400.

### Custom collections (`kind = 'custom'`)

- Created, renamed, and deleted freely by the user.
- Each is a **named union of one or more directories**, tracked in
  `collection_directories`. The collection's videos are the union of the
  videos in those directories (filtered by `missing = 0`).
- Custom collections update automatically as their directories update — no
  maintenance step, no re-scan required. Adding a new file to a referenced
  directory makes it appear in every custom collection including that
  directory on the next page load.
- Individual videos cannot be added to a custom collection. Per-video
  curation is deferred to a future "playlists" feature.

## Home page

Home (`/`) shows two sections:

- **Directories**: directory collections with `hidden = 0`.
- **My Collections**: custom collections.

Each card shows name, video count, and a small mosaic of recent thumbnails.

Creating a new custom collection opens a modal that asks for a name and a
checklist of directories to include. An empty checklist produces an empty
collection that can be populated later from its detail page.

## Custom collection page

A custom collection page shows, in order:

- The header (name, video count, Random / Rename / Delete).
- A chip row listing the directories currently included in the collection.
  Each chip links to that directory's collection page and has a × button to
  remove it. At the end of the row is an "+ Add directory…" dropdown listing
  eligible directories (active and not already included).
- The video grid — the union of videos across included directories.

## Soft-removed directories

When a directory is soft-removed (`directories.removed = 1`):

- Any in-flight background jobs for videos in this directory are **aborted**:
  the worker task is cancelled via `AbortHandle`, and the ffmpeg/ffprobe child
  process is terminated via `kill_on_drop(true)` on the `Command`. Aborted job
  rows are deleted outright.
- Its directory collection is flagged `hidden = 1` and disappears from Home.
- All videos in that directory are flagged `missing = 1`, which excludes them
  from all collection listings (directory and custom alike).
- `watch_history` is preserved. `collection_directories` links to this
  directory are preserved — they become inert while the directory is removed
  and automatically start contributing again when it is re-added.
- Cached thumbnails and previews are left on disk.

When re-added:

- `directories.removed` is cleared, `collections.hidden` is cleared, and the
  collection's existing `name` is preserved (so user label edits survive
  re-adds).
- The scanner runs; videos whose stat signature still matches the stored row
  have `missing` cleared and their `thumbnail_ok` / `preview_ok` flags
  preserved. The cache-verification pass re-enqueues jobs only for videos
  whose cache files are no longer on disk. See [`04-scanner.md`](./04-scanner.md).
- Custom collections that referenced this directory automatically show its
  videos again on the next page load.

## Hard-removed directories

Hard-remove is an explicit user action (`DELETE /api/directories/:id?mode=hard`).
It is irreversible:

- Any in-flight background jobs for videos in this directory are **aborted**
  and their child processes terminated, same as soft-remove.
- All thumbnail and preview cache files for videos in the directory are
  removed from disk (best-effort).
- All `jobs` rows referencing those videos are deleted.
- The `directories` row is deleted, which cascades via FK to `videos`,
  `watch_history`, the directory's own `collections` row, and any
  `collection_directories` rows referencing this directory.
- Custom collections themselves remain; they simply lose any reference they
  had to this directory.

Never hard-remove as a side effect of another action. It must be user-initiated
through the `mode=hard` API path.

## Random

Each collection page has a **🎲 Random** button (hotkey `R`) that:

1. Calls `GET /api/collections/:id/random` → `{ video_id }`. For directory
   collections this is uniform random over `videos` with that `directory_id`
   and `missing = 0`; for custom collections it is uniform random over
   `videos` whose `directory_id` appears in any of the collection's
   `collection_directories` rows (again with `missing = 0`).
2. Navigates to `/videos/:video_id?cid=:id`.
3. No auto-launch; the user decides on the detail page.

Empty collection → endpoint returns 404; UI shows a message.
