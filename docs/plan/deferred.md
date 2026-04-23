# Deferred features

Ideas out of MVP scope. Add to this file whenever a useful idea comes up during implementation.

## Features

- **Playlists** — a per-video-curated list, distinct from custom collections.
  Collections are sets of directories that auto-update with the scanner;
  playlists would let the user hand-pick individual videos and reorder them.
  Would reintroduce something like the old `collection_videos` table but
  scoped to an explicit "playlist" kind.
- **Search** across collections (by filename and, later, by extracted metadata).
- **Tags** and **smart collections** (rule-based).
- **Rename detection** — match a new file to a missing one by `(size, mtime)` before treating
  as a new video, to preserve watch history across moves.
- **In-browser playback** with a custom scrubber driven by the WebVTT. We'd still keep mpv
  as the primary target; this would be a fallback.
- **File listing toggle** in the directory picker (currently directories only).
- **Per-job progress breakdown UI** in the header (queue depth, current kind).
- **Retry failed jobs** endpoint / button.
- **Automated `vidviewer restore-backup`** subcommand.
- **Detach** option on directory remove — keep the `directories` row, mark everything missing,
  but don't cascade on re-add.
- **mpv hover-thumbnail integration** — make the existing sprite sheet + WebVTT
  show up when hovering the mpv seek bar, so in-mpv scrubbing has the same
  preview affordance as the browser grid. Requires shipping a small bundled
  Lua script (passed via `--script=...`/`--script-opts=...` at launch, no
  user install step), plus a new worker job that splits the existing sprite
  sheet into per-tile raw BGRA files (mpv's `overlay-add` doesn't take JPEG).
  Script would speak both thumbfast's IPC protocol (covers uosc / ModernX /
  other modern OSC replacements on mpv ≤ 0.41) and the stock-OSC Preview
  API landing in mpv 0.42+. Cost: ~150 lines of Lua, ~3 MB of BGRA per
  video on disk, a new `mpv_preview_tiles` job kind, and touching
  `src/player/`, `src/jobs/`, `migrations/`, and `docs/design/{03,06,08}`.
  Users on vanilla stock OSC (mpv ≤ 0.41 without uosc) would still see
  nothing unless we also bundle po5's `vanilla-osc` fork. Deferred because
  the browser grid already has working hover-scrub and most mpv users who
  care about in-mpv thumbnails already run uosc + thumbfast.

## Explicitly out of scope

- Windows or macOS support.
- Multi-user accounts, auth, LAN exposure.
- Cloud / remote libraries.
- In-app transcoding.
