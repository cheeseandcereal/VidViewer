# 11 — UTF-8 and i18n

Last updated: 2026-04-22

VidViewer is designed to handle full UTF-8 including CJK (Chinese, Japanese, Korean) text
correctly throughout. Linux-only scope simplifies this considerably compared to cross-platform:
macOS NFD normalization and Windows UTF-16 are non-issues.

## Filesystem

- All paths are `PathBuf` internally.
- When inserting into the DB, paths are converted via `path.to_string_lossy()`. If the result
  differs from the raw bytes, a `tracing::warn!` is emitted naming the file so the user can
  rename it if they care about round-trip correctness.
- Non-UTF-8 filenames are rare on modern Linux systems.

## SQLite

- `PRAGMA encoding = 'UTF-8'` is set at DB init.
- Text columns store raw UTF-8. No normalization.

## HTTP

- HTML responses always carry `Content-Type: text/html; charset=utf-8`.
- JSON responses are UTF-8 by default via `serde_json`.
- The base template declares `<meta charset="utf-8">`.
- Any user or filesystem text placed into a URL path or query string is percent-encoded via
  the helper in `src/util/url.rs`. Do not manually construct URLs with raw filenames.

## Subprocess arguments

- File paths are passed to `ffmpeg`/`ffprobe`/`mpv` via `Command::arg(&Path)` which forwards
  the raw `OsStr` bytes to `execvp`. Never build shell strings; this avoids CJK / whitespace
  / quoting bugs.

## Fonts

The default font stack targets CJK legibility without bundling fonts:

```
system-ui, "Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR",
"Noto Sans", "DejaVu Sans", sans-serif
```

## Logs

`tracing` writes UTF-8 to stdout by default and to JSON with `LOG_FORMAT=json`. No
normalization; user-visible strings (filenames, labels) appear verbatim.
