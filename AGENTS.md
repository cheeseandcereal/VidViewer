# VidViewer — Agent Guide

This file gives AI agents (and humans) the context needed to be productive on this codebase quickly.

## What this is

**VidViewer** is a local-first, self-hosted web app for browsing a personal video library.
It scans configured directories, generates thumbnails and hover-scrub previews, organizes videos
into collections (directory-based and custom), and launches selected videos in `mpv` (not in-browser).
Watch history and resume positions are captured via `mpv`'s JSON IPC.

## Platform and stack

- **Target OS:** Linux only. Never add macOS or Windows support.
- **Language:** Rust stable (MSRV 1.82, declared via `rust-version` in `Cargo.toml`)
- **HTTP:** `axum` + `tower-http`
- **Templates:** `askama` (compile-time checked)
- **Database:** SQLite via `sqlx` (compile-time query checking in offline mode)
- **Runtime:** `tokio`
- **Logging:** `tracing` (pretty by default, JSON via `LOG_FORMAT=json`)
- **External tools (must be on `PATH`):** `ffmpeg`, `ffprobe`, `mpv`
- **Frontend:** hand-written CSS and vanilla JS. **No build step, ever.**

## Where to look first

| You want to... | Read this |
|---|---|
| Understand the project at a glance | `docs/design/01-overview.md` |
| Understand components and request flow | `docs/design/02-architecture.md` |
| Work with the DB schema | `docs/design/03-data-model.md` |
| Understand the scanner | `docs/design/04-scanner.md` |
| Add a job kind | `docs/agents/adding-a-job-kind.md` |
| Add a new page | `docs/agents/adding-a-page.md` |
| Change the schema | `docs/agents/changing-the-schema.md` |
| Debug a failing build/test/run | `docs/agents/debugging.md` |
| See what's left in the MVP | `docs/plan/mvp-build-order.md` |
| See post-MVP ideas | `docs/plan/deferred.md` |

## Conventions

### Commits
- Use Conventional Commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`.
- Every commit should leave `just check && just test` passing.

### Code style
- Use `sqlx` with `query!` / `query_as!` macros (compile-time SQL verification).
  Keep `.sqlx/` metadata committed so offline compilation works.
- Use `tracing` for logs. Never `println!` or `eprintln!` in library code.
- Always pass filesystem paths via `Command::arg(&Path)`, never via shell strings.
- Always percent-encode text placed into URL paths or query strings. Helpers live in `src/util/url.rs`.
- Set `Content-Type: text/html; charset=utf-8` on all HTML responses (default via Askama integration, but assert explicitly in tests).
- Prefer `tokio::fs` over `std::fs` in request and job paths.
- Use `anyhow::Result` at boundaries; use typed errors (`thiserror`) where they cross module lines.
- No bare `?` on IO — add `.context("...")`.
- Use typed IDs: `VideoId`, `CollectionId`, `DirectoryId` (see `src/ids.rs`). Never pass bare `String`/`i64` IDs.
- Use the `Clock` trait (`src/clock.rs`) for any `now()` call. Inject it so tests are deterministic.
- Prefer straightforward imperative code over clever iterator chains.
- Target ≤ 400 LOC per file; split earlier rather than later.

### External process boundaries
All code that shells out to ffmpeg/ffprobe/mpv must go through a trait:
- `Player` trait → `MpvPlayer` (real) / `MockPlayer` (tests). See `src/player/`.
- `VideoTool` trait → `FfmpegTool` (real) / `MockVideoTool` (tests). See `src/video_tool/`.

Tests should use the mocks unless they are specifically exercising the real binaries (behind an integration-test feature flag).

### Schema migrations
- Migrations live in `migrations/NNNN_description.sql`.
- **Never edit a committed migration.** Always add a new one.
- When changing the schema, update `docs/design/03-data-model.md` in the same change.
- A backup is taken automatically before any migration runs at startup. See `docs/design/14-migrations.md`.

### Documentation
- **Any** design change must update the relevant `docs/design/*.md` in the same change.
- When a build step from `docs/plan/mvp-build-order.md` is completed, check it off.
- When new ideas come up that are out of MVP scope, add them to `docs/plan/deferred.md`.
- Per-subsystem README files live alongside their code (e.g. `src/player/README.md`) for local nuances that don't rise to the level of a design doc.

## Invariants

These must remain true; violating them means a bug:

1. `(directory_id, relative_path)` uniquely identifies a video row.
2. `video_id` is stable across file content changes — only the content hash (`size`, `mtime`) and derived assets change. URLs to thumbnails and previews are cache-busted via `?v=<updated_at_epoch>`.
3. Directory collections (`collections.kind = 'directory'`) can only be mutated via their `name` field. All other mutations must return `400` at the API boundary.
4. A video marked `missing = 1` is removed from its directory collection's `collection_videos` rows but remains in any custom collection memberships (rendered with a "missing" badge).
5. Soft-removed directories (`directories.removed = 1`) are skipped by the scanner. Their directory collection is flagged `hidden = 1` and not shown in listings. Re-adding the same path un-hides and un-removes in place.
6. Watch history (`watch_history` rows) is never deleted as a side effect of directory soft-remove. Only hard-deleting a video (currently never done by the app) would cascade-delete history.

## What not to do

- Do not add Windows or macOS support.
- Do not add a frontend build step (no npm, webpack, vite, etc.).
- Do not add auth or LAN exposure — this is localhost-only.
- Do not add in-browser playback in v1.
- Do not edit committed migrations.
- Do not delete rows on directory removal — soft-remove only.

## Running and testing

| Task | Command |
|---|---|
| Format | `just fmt` |
| Lint | `just lint` |
| Type-check | `just check` |
| Unit + integration tests | `just test` |
| Run the server | `just run` |
| Environment sanity | `just doctor` |
| End-to-end smoke | `just smoke` |
| Refresh sqlx offline metadata | `just prepare-sqlx` |

## How to verify your changes

Before considering a change complete, run:
```
just fmt
just lint
just check
just test
```
All four must pass.

If you changed the schema, also:
- Confirm a new migration file was added (not an edit to an existing one).
- Run `just prepare-sqlx` and commit the updated `.sqlx/` metadata.
- Update `docs/design/03-data-model.md`.

If you changed the HTTP API, update `docs/design/09-http-api.md`.

If you changed behavior described in any `docs/design/*.md` file, update that file.
