# VidViewer

A local-first, self-hosted web app for browsing a personal video library on Linux.

Scan one or more directories, generate thumbnails and YouTube-style hover-scrub previews,
organize videos into directory-backed and custom collections, launch selected videos in
your local `mpv` player, and track watch history and resume positions via mpv's JSON IPC.

## Design goals

- **Local-first.** Runs on `localhost`, no auth, no LAN exposure.
- **External playback.** Videos launch in `mpv`, not in-browser. One less thing to debug.
- **Unicode correct.** Full UTF-8 including CJK filenames and titles.
- **Boring infrastructure.** Single binary, single SQLite file, no frontend build step.

For a deeper tour, start at [`docs/README.md`](./docs/README.md).

## Requirements

- Linux (macOS and Windows are not supported; see [AGENTS.md](./AGENTS.md)).
- Rust stable (MSRV 1.82).
- `ffmpeg`, `ffprobe`, `mpv` on your `PATH`.

## Quick start

```sh
# Install deps (example, Arch Linux)
sudo pacman -S ffmpeg mpv

# Build and run
cargo run --release

# First launch writes default config to ~/.config/vidviewer/config.toml
# Data lives at ~/.local/share/vidviewer/
```

Open http://localhost:7878 in your browser, click **Settings → Add Directory**, point it
at a folder of videos, and wait for the scanner to finish.

## Features

- Add/remove directories via the UI (soft-remove preserves watch history).
- Auto-generated **directory collections** plus user-curated **custom collections**.
- **Poster thumbnails** and a **preview tile sheet + WebVTT** for hover-scrub over the seek bar.
- **Click to play in mpv.** Progress is persisted; relaunching resumes where you left off.
- **🎲 Random button** on every collection (hotkey: `R`). Opens the detail page for the pick
  rather than auto-playing, so you can preview before committing.
- **History page** showing resume progress and watch counts.
- **Videos in custom collections** survive directory soft-removal (shown with a "missing" badge).

## Development

Common tasks via [`just`](https://github.com/casey/just):

```sh
just fmt           # cargo fmt
just lint          # clippy, warnings as errors
just check         # cargo check
just test          # unit + integration
just run           # start the server
just doctor        # environment sanity check (ffmpeg / mpv / DB)
just build         # release binary at target/release/vidviewer
just install       # cargo install --path . (copies to ~/.cargo/bin)
```

Release builds statically link all Rust code and crates; Linux system libraries
(glibc, libm, libpthread) link dynamically. Templates and SQL migrations are
embedded into the binary at compile time.

See [AGENTS.md](./AGENTS.md) for conventions (Conventional Commits, schema migration rules,
UTF-8 hygiene, trait-bound external process boundaries) and [`docs/`](./docs/) for the full
design documentation.

## Layout

```
src/            Rust source
templates/      Askama HTML templates
static/         Hand-written CSS + vanilla JS (no build step)
migrations/     Append-only SQLite migrations
docs/           Design and agent documentation
tests/          Integration tests
```

## License

This is free and unencumbered software released into the public domain under
[The Unlicense](./UNLICENSE).
