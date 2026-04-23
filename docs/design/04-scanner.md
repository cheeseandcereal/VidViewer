# 04 — Scanner

Last updated: 2026-04-22

The scanner keeps the DB aligned with configured directories on disk. It is deliberately
cheap on no-op runs so it can run at startup as well as on demand.

## Invocation

- At startup (if `scan_on_startup = true` in config).
- On user click of the **Rescan** button (global or per-directory).
- After a directory is added via the Settings UI.

Each invocation returns a `scan_id` that the UI can poll via `GET /api/scan/status?scan_id=...`.

## Algorithm

For each directory with `removed = 0`:

1. `SELECT relative_path, size_bytes, mtime_unix, missing, id FROM videos WHERE directory_id = ?`
   into a hashmap keyed by `relative_path`.
2. Walk the directory tree (tokio-friendly; yields periodically). Filter to video extensions.
3. For each file:
   - Stat → `(size_bytes, mtime_unix)`.
   - If `relative_path` not in map:
     - Insert video row with fresh `video_id` UUID, `missing = 0`.
     - Insert `collection_videos` row for the directory's collection.
     - Enqueue `probe` job. `thumbnail` and `preview` jobs are enqueued when `probe` completes
       (they need `duration_secs`).
   - Else if `(size_bytes, mtime_unix)` changed:
     - Update the existing row. Clear `thumbnail_ok`, `preview_ok`.
     - Enqueue `probe` job.
     - If previously `missing = 1`, clear the flag and re-insert the `collection_videos` row.
   - Else:
     - Remove `relative_path` from the map (marker for "still present") and skip.
4. For entries remaining in the map (not found on disk):
   - Mark `missing = 1`.
   - Delete the matching `collection_videos` row for the directory collection.
   - Leave custom collection memberships intact. Leave `watch_history` intact.
5. **Cache verification.** For each video that survived the walk and is flagged
   `thumbnail_ok = 1` or `preview_ok = 1`, check whether the expected cache file
   exists on disk:
   - `cache/thumbs/<video_id>.jpg` for thumbnails.
   - `cache/previews/<video_id>.jpg` **and** `cache/previews/<video_id>.vtt` for previews
     (only checked when `duration_secs` is known and positive).

   If an expected file is missing, clear the corresponding flag and enqueue a new job.
   This is the recovery path when a user has cleared or moved the cache directory.
   Counters `recovered_thumbnail_jobs` / `recovered_preview_jobs` on `ScanReport` track
   how many recoveries happened.

## Filename handling

- Linux filenames are raw bytes; the scanner converts to `String` via `path.to_string_lossy()`
  and logs a warning if the conversion changed bytes. Such files are still indexed, but any
  subsequent operations use the lossy-converted path, which may not round-trip exactly.
- In practice, >99% of real-world filenames are valid UTF-8.

## Extensions recognized

Default list: `mp4 mkv webm mov avi m4v flv wmv mpg mpeg ts m2ts`.
Hardcoded for v1; configurable is deferred.

## Dry-run mode

`vidviewer scan --dry-run [<dir_id>]` performs the walk and diff phases and prints the
planned inserts / updates / missings without writing anything. Useful for:

- Verifying that a directory layout is indexed as expected.
- Debugging scanner behavior in a support context.

## Re-adding a soft-removed directory

If the user re-adds a path matching an existing `directories.removed = 1` row:

- That row's `removed` is cleared.
- The matching `collections.hidden` is cleared.
- The scanner runs normally, un-missing previously-seen files and re-adding their membership.

See [`07-collections.md`](./07-collections.md) for collection-side effects.
