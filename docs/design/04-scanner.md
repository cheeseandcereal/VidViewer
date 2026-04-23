# 04 — Scanner

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
2. Walk the directory **non-recursively** (`max_depth = 1`). Only files
   sitting directly inside the configured directory are considered; any
   subdirectory tree is ignored. A user who wants a nested folder indexed
   adds it as its own top-level directory in Settings.
3. For each file:
   - Stat → `(size_bytes, mtime_unix)`.
   - If `relative_path` not in map:
     - Insert video row with fresh `video_id` UUID, `missing = 0`.
     - Insert `collection_videos` row for the directory's collection.
     - Enqueue `probe` job. `thumbnail` and `preview` jobs are enqueued when `probe` completes
       (they need `duration_secs`).
   - Else if `(size_bytes, mtime_unix)` changed (content on disk differs from the stored row):
     - Update the existing row. Clear `thumbnail_ok`, `preview_ok`.
     - Enqueue `probe` job.
     - If previously `missing = 1`, also re-insert the `collection_videos` row.
   - Else if `missing = 1` (stat matches but the row was flagged missing, e.g. because
     the directory was previously soft-removed):
     - Set `missing = 0` and touch `updated_at`.
     - Re-insert the `collection_videos` row for the directory collection.
     - **Preserve** `thumbnail_ok` / `preview_ok`. Do not enqueue probe. The cache
       verification pass below will detect any missing cache files and re-enqueue
       only what's actually needed.
   - Else (truly unchanged): no-op.
4. For entries remaining in the map (not found on disk):
   - Mark `missing = 1`.
   - Delete the matching `collection_videos` row for the directory collection.
   - Leave custom collection memberships intact. Leave `watch_history` intact.
5. **Cache verification.** For each video that survived the walk, check that the
   expected derived assets exist on disk and that the corresponding DB flag is set.
   The invariant is **(flag = 1) ⇔ (file exists)**. If either side is off, clear
   the flag and enqueue a fresh job.

   - `cache/thumbs/<video_id>.jpg` for thumbnails. Checked unconditionally.
   - `cache/previews/<video_id>.jpg` **and** `cache/previews/<video_id>.vtt` for
     previews. Only checked when `duration_secs` is known and positive (previews
     require duration).

   This pass catches three recovery scenarios:
   1. The user cleared or moved the cache directory (flag = 1, file missing).
   2. A past job failed or was aborted (flag = 0, file missing); the scan
      re-enqueues rather than waiting for another event to trigger regeneration.
   3. Re-adding a soft-removed directory where the cache is still intact; the
      invariant holds on both sides, and nothing is enqueued.

   Counters `recovered_thumbnail_jobs` / `recovered_preview_jobs` on `ScanReport`
   track how many recoveries happened. `enqueue_on` is idempotent, so if a job
   is already pending or running for the same `(kind, video_id)`, no duplicate
   row is inserted.

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
- The matching `collections.hidden` is cleared (the collection's `name` is preserved,
  so user label edits survive).
- The scanner runs normally. For files whose stat signature matches the stored row,
  the un-missing sub-branch (§3) preserves `thumbnail_ok` and `preview_ok`. The
  cache-verification pass (§5) then re-enqueues thumbnail or preview jobs only for
  videos whose cache files are no longer on disk. Videos whose files changed on
  disk during the hidden period take the regular change path: flags cleared, probe
  re-enqueued.

This means re-adding a soft-removed directory is cheap if the cache is intact, and
only does the work that's actually needed when the cache has been cleared.

See [`07-collections.md`](./07-collections.md) for collection-side effects.
