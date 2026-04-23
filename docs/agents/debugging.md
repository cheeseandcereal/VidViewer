# Agent playbook — Debugging

Last updated: 2026-04-22

## Quick checks

```sh
just doctor   # verifies ffmpeg, ffprobe, mpv are on PATH; DB writable; cache dirs writable
just check    # cargo check with clippy
just test     # unit + integration tests
```

## Logs

`tracing` writes to stdout. Defaults to pretty format.

```sh
LOG_LEVEL=debug just run
LOG_FORMAT=json  just run | jq .
```

Every HTTP request is instrumented with a span carrying `method`, `path`, and relevant IDs.

## Job queue state

Enable the `/debug` endpoint via `enable_debug_endpoint = true` in config; then:

```sh
curl http://localhost:7878/debug | jq .
```

Returns the pending/running/failed job counts, scanner status, and active mpv sessions.

## Manually running ffmpeg

Because all subprocess args are passed via `Command::arg(&Path)`, you can reproduce what the
app runs by copying the logged command line. If the app logs `ffmpeg -ss 5 -i /path/to/file.mp4 ...`,
running the same in a shell (with appropriate quoting) should produce the same behavior.

## Restoring from backup

Backups live at `~/.local/share/vidviewer/backups/`. To restore:

1. Stop the server.
2. ```sh
   mv ~/.local/share/vidviewer/vidviewer.db ~/.local/share/vidviewer/vidviewer.db.broken
   cp ~/.local/share/vidviewer/backups/vidviewer-<timestamp>-pre-migration-v<N>.db \
      ~/.local/share/vidviewer/vidviewer.db
   ```
3. Start the server with a binary compatible with schema version `<N>`.

## Scanner dry-run

```sh
cargo run -- scan --dry-run           # all non-removed directories
cargo run -- scan --dry-run 3         # just directory id 3
```

Reports planned inserts/updates/missings without writing.
