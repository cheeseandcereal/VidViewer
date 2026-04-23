# 09 — HTTP API

All responses are JSON unless otherwise noted. HTML page responses have
`Content-Type: text/html; charset=utf-8`.

## Pages (HTML)

| Method | Path | Description |
|---|---|---|
| GET | `/` | Home — collections listing |
| GET | `/collections/:id` | Collection page |
| GET | `/videos/:id` | Video detail page |
| GET | `/history` | Watch history |
| GET | `/settings` | Settings |
| GET | `/healthz` | Plain-text `ok`; for uptime checks |

## Static cache

| Method | Path | Description |
|---|---|---|
| GET | `/thumbs/:id.jpg` | Poster thumbnail |
| GET | `/previews/:id.jpg` | Preview tile sheet |
| GET | `/previews/:id.vtt` | Preview WebVTT |

All include a `?v=<updated_at_epoch>` cache-bust in generated links.

## Filesystem picker

| Method | Path | Description |
|---|---|---|
| GET | `/api/fs/list?path=<abs_path>` | List subdirectories of `path`. |

Response:

```json
{
  "path": "/home/user/Videos",
  "parent": "/home/user",
  "entries": [
    { "name": "Movies", "path": "/home/user/Videos/Movies", "is_dir": true, "readable": true }
  ]
}
```

Errors: `path_not_absolute`, `path_not_found`, `path_not_a_directory`, `path_not_readable`.

## Directories

| Method | Path | Description |
|---|---|---|
| GET | `/api/directories` | List all directories including removed ones (UI filters). |
| POST | `/api/directories` | Body `{ path, label? }`. Un-hides if an existing matching row has `removed=1`. |
| PATCH | `/api/directories/:id` | Body `{ label }`. Also updates the directory collection's `name`. |
| DELETE | `/api/directories/:id?mode=soft\|hard` | Soft-remove (default) preserves history and cache. Hard-remove cascades through `videos`, `watch_history`, the directory's `collections` row, and any `collection_directories` rows referencing this directory, and deletes cache files on disk. |

Soft remove returns `204 No Content`.

Hard remove returns `200 OK` with:

```json
{ "deleted_videos": 12, "deleted_cache_files": 36, "deleted_jobs": 0 }
```

A mode other than `soft` or `hard` returns `400 Bad Request` with
`{ "error": "bad_mode" }`.

POST errors: `path_not_absolute`, `path_not_found`, `path_not_a_directory`, `path_not_readable`, `path_already_added` (non-removed duplicate).

## Collections

Custom collections are named unions of one or more directories. Their video
membership is computed on read from those directories. Per-video curation is
not supported — that is deferred to a future playlists feature.

| Method | Path | Description |
|---|---|---|
| GET | `/api/collections` | `?kind=custom\|directory` filter optional. |
| POST | `/api/collections` | Body `{ name, directory_ids?: [int, …] }`. Creates a custom collection and (optionally) seeds it with the given directories. Each directory must exist and not be soft-removed. |
| PATCH | `/api/collections/:id` | Body `{ name }`. Rename. |
| DELETE | `/api/collections/:id` | Custom only. Returns 400 for directory collections. |
| GET | `/api/collections/:id/videos` | Videos in the collection (computed on read). |
| GET | `/api/collections/:id/directories` | Directories included in a custom collection. Returns `[]` for directory collections. |
| POST | `/api/collections/:id/directories` | Body `{ directory_id }`. Custom only. 400 if the directory is unknown or soft-removed. |
| DELETE | `/api/collections/:cid/directories/:did` | Custom only. Removes the link; videos and history are unaffected. |
| GET | `/api/collections/:id/random` | `{ video_id }` or 404 if empty. |

## Videos

| Method | Path | Description |
|---|---|---|
| GET | `/api/videos/:id` | Full video detail. |
| POST | `/api/videos/:id/play` | Optional `?start=<secs>`. Launches mpv; returns 202 with `{ session_id }`. |

## Scan / jobs

| Method | Path | Description |
|---|---|---|
| POST | `/api/scan` | Optional `?dir_id=...`. Returns `{ scan_id }`. |
| GET | `/api/scan/status?scan_id=...` | Phase, counts. |

Scan status shape:

```json
{
  "scan_id": "...",
  "dir_id": 3,
  "phase": "walking",
  "files_seen": 1423,
  "new_videos": 873,
  "changed_videos": 0,
  "missing_videos": 0,
  "error": null
}
```

## History

| Method | Path | Description |
|---|---|---|
| GET | `/api/history` | Chronological list. |
| DELETE | `/api/history/:video_id` | Clear one entry. |

## Debug

| Method | Path | Description |
|---|---|---|
| GET | `/debug` | Localhost-only, gated by `config.enable_debug_endpoint = true`. Dumps job queue, scanner state, active mpv sessions. |
