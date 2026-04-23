# 01 — Overview

## Purpose

VidViewer is a local-first, self-hosted web app for browsing a personal video library on Linux.
It is designed to work like a minimal YouTube clone pointed at local directories: thumbnails,
hover-scrub previews, collections, a Random button, and watch history — but all playback happens
in an external `mpv` process, not in the browser.

## Goals

- **Instant browsing**: fast grid of thumbnails, hover-scrub previews.
- **Organization**: directory-backed collections plus user-curated custom collections.
- **Discovery**: a Random button inside any collection.
- **Resume**: watch history and last position captured from `mpv` via JSON IPC.
- **Low friction**: one binary, one SQLite file, zero external services, zero auth.
- **Unicode-correct**: full UTF-8 including CJK filenames and titles.
- **Agent-friendly**: deterministic builds, structured logs, typed IDs, trait-bounded external processes.

## Non-goals (for v1)

- In-browser video playback (explicitly external via `mpv`).
- Windows or macOS support. Linux only.
- Multi-user, authentication, or LAN exposure. Localhost only.
- Cloud sync, remote libraries, or transcoding.
- Rename detection across moves. A moved file looks like delete + create to the scanner.

## Users and usage

The user is expected to be the person running the binary on their own machine. They configure
directories to scan via the web UI (Settings), browse collections on Home, click a video to
launch it in `mpv`, and optionally press R or click Random within a collection to discover
something to watch.

## High-level shape

- A single Rust process that:
  - Hosts an Axum HTTP server on `localhost:<port>` (default `7878`).
  - Reads and writes a SQLite database.
  - Spawns `ffmpeg` / `ffprobe` subprocesses to generate thumbnails, metadata, and preview sprite sheets.
  - Spawns `mpv` when the user plays a video, and connects to its JSON IPC Unix socket to track progress.
- A browser points at the server and renders the UI.
- No frontend build step; templates (Askama) and a small amount of vanilla JS/CSS serve the UI directly.

See [`02-architecture.md`](./02-architecture.md) for the component diagram and request flow.
