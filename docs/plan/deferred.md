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

## Explicitly out of scope

- Windows or macOS support.
- Multi-user accounts, auth, LAN exposure.
- Cloud / remote libraries.
- In-app transcoding.
